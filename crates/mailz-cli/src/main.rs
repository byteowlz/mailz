use std::env;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;
use env_logger::fmt::WriteStyle;
use log::{LevelFilter, debug, info};
use serde::Serialize;

use mailz_core::paths::expand_str_path;
use mailz_core::{
    AgentIdentity, AppConfig, AppPaths, CreateAgent, CreateFileReservation, DaemonState,
    Importance, SendMessage, Storage, clear_daemon_state, default_cache_dir, default_parallelism,
    load_agent_identity, load_daemon_state, save_agent_identity, save_daemon_state,
};

const APP_NAME: &str = env!("CARGO_PKG_NAME");

fn main() {
    if let Err(err) = try_main() {
        let _ = writeln!(io::stderr(), "{err:?}");
        std::process::exit(1);
    }
}

fn try_main() -> Result<()> {
    let cli = Cli::parse();

    let mut ctx = RuntimeContext::new(cli.common.clone())?;
    ctx.init_logging()?;
    debug!("resolved paths: {:#?}", ctx.paths);

    match cli.command {
        Command::Run(cmd) => handle_run(&mut ctx, cmd),
        Command::Init(cmd) => handle_init(&ctx, cmd),
        Command::Config { command } => handle_config(&ctx, command),
        Command::Project { command } => handle_project(&ctx, command),
        Command::Agent { command } => handle_agent(&ctx, command),
        Command::Send(cmd) => handle_send(&ctx, cmd),
        Command::Inbox(cmd) => handle_inbox(&ctx, cmd),
        Command::Read(cmd) => handle_read(&ctx, cmd),
        Command::Ack(cmd) => handle_ack(&ctx, cmd),
        Command::Search(cmd) => handle_search(&ctx, cmd),
        Command::Reserve(cmd) => handle_reserve(&ctx, cmd),
        Command::Release(cmd) => handle_release(&ctx, cmd),
        Command::Reservations(cmd) => handle_reservations(&ctx, cmd),
        Command::Check(cmd) => handle_check(&ctx, cmd),
        Command::Gc(cmd) => handle_gc(&ctx, cmd),
        Command::Watch(cmd) => handle_watch(&ctx, cmd),
        Command::Daemon { command } => handle_daemon(&ctx, command),
        Command::Completions { shell } => handle_completions(shell),
    }
}

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "Mailz CLI for agent coordination.",
    propagate_version = true
)]
struct Cli {
    #[command(flatten)]
    common: CommonOpts,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Clone, Args)]
pub struct CommonOpts {
    /// Override the config file path
    #[arg(long, value_name = "PATH", global = true)]
    pub config: Option<PathBuf>,
    /// Reduce output to only errors
    #[arg(short, long, action = clap::ArgAction::SetTrue, global = true)]
    pub quiet: bool,
    /// Increase logging verbosity (stackable)
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,
    /// Enable debug logging (equivalent to -vv)
    #[arg(long, global = true)]
    pub debug: bool,
    /// Enable trace logging (overrides other levels)
    #[arg(long, global = true)]
    pub trace: bool,
    /// Output machine readable JSON
    #[arg(long, global = true, conflicts_with = "yaml")]
    pub json: bool,
    /// Output machine readable YAML
    #[arg(long, global = true)]
    pub yaml: bool,
    /// Disable ANSI colors in output
    #[arg(long = "no-color", global = true, conflicts_with = "color")]
    pub no_color: bool,
    /// Control color output (auto, always, never)
    #[arg(long, value_enum, default_value_t = ColorOption::Auto, global = true)]
    pub color: ColorOption,
    /// Do not change anything on disk
    #[arg(long = "dry-run", global = true)]
    pub dry_run: bool,
    /// Assume "yes" for interactive prompts
    #[arg(short = 'y', long = "yes", alias = "force", global = true)]
    pub assume_yes: bool,
    /// Never prompt for input; fail if confirmation would be required
    #[arg(long = "no-input", global = true)]
    pub no_input: bool,
    /// Maximum seconds to allow an operation to run
    #[arg(long = "timeout", value_name = "SECONDS", global = true)]
    pub timeout: Option<u64>,
    /// Override the degree of parallelism
    #[arg(long = "parallel", value_name = "N", global = true)]
    pub parallel: Option<usize>,
    /// Disable progress indicators
    #[arg(long = "no-progress", global = true)]
    pub no_progress: bool,
    /// Emit additional diagnostics for troubleshooting
    #[arg(long = "diagnostics", global = true)]
    pub diagnostics: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ColorOption {
    Auto,
    Always,
    Never,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Execute the CLI's primary behavior
    Run(RunCommand),
    /// Create config directories and default files
    Init(InitCommand),
    /// Inspect and manage configuration
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Manage projects
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    /// Manage agents
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    /// Send a message
    Send(SendCommand),
    /// List inbox messages
    Inbox(InboxCommand),
    /// Read a message (marks as read)
    Read(ReadCommand),
    /// Acknowledge a message
    Ack(AckCommand),
    /// Search messages
    Search(SearchCommand),
    /// Reserve files
    Reserve(ReserveCommand),
    /// Release reservations
    Release(ReleaseCommand),
    /// List active reservations
    Reservations(ReservationsCommand),
    /// Check files for active reservations
    Check(CheckCommand),
    /// Run garbage collection
    Gc(GcCommand),
    /// Watch for new messages
    Watch(WatchCommand),
    /// Manage background daemon
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Generate shell completions
    Completions {
        #[arg(value_enum)]
        shell: Shell,
    },
}

#[derive(Debug, Clone, Args)]
struct RunCommand {
    /// Named task to execute
    #[arg(value_name = "TASK", default_value = "default")]
    task: String,
    /// Override the profile to run under
    #[arg(long, value_name = "PROFILE")]
    profile: Option<String>,
}

#[derive(Debug, Clone, Args)]
struct InitCommand {
    /// Recreate configuration even if it already exists
    #[arg(long = "force")]
    force: bool,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Output the effective configuration
    Show,
    /// Print the resolved config file path
    Path,
    /// Print all resolved paths (config, data, state, cache)
    Paths,
    /// Print the JSON schema for the config file
    Schema,
    /// Regenerate the default configuration file
    Reset,
}

#[derive(Debug, Subcommand)]
enum ProjectCommand {
    /// List all projects
    List,
    /// Create or ensure a project
    Create(ProjectCreate),
    /// Show project details
    Info(ProjectInfo),
    /// Delete a project
    Delete(ProjectDelete),
}

#[derive(Debug, Args)]
struct ProjectCreate {
    /// Path to the project
    #[arg(value_name = "PATH")]
    path: String,
    /// Optional project name/slug
    #[arg(long, value_name = "NAME")]
    name: Option<String>,
}

#[derive(Debug, Args)]
struct ProjectInfo {
    /// Project name/slug
    #[arg(value_name = "NAME")]
    name: String,
}

#[derive(Debug, Args)]
struct ProjectDelete {
    /// Project name/slug
    #[arg(value_name = "NAME")]
    name: String,
}

#[derive(Debug, Subcommand)]
enum AgentCommand {
    /// Register a new agent
    Register(AgentRegister),
    /// List agents in a project
    List(AgentList),
    /// Show agent details
    Info(AgentInfo),
    /// Show current agent identity
    Whoami,
}

#[derive(Debug, Args)]
struct AgentRegister {
    /// Optional agent name
    #[arg(long, value_name = "NAME")]
    name: Option<String>,
    /// Project name/slug
    #[arg(long, value_name = "PROJECT")]
    project: Option<String>,
    /// Program identifier for the agent
    #[arg(long, default_value = APP_NAME)]
    program: String,
    /// Model identifier for the agent
    #[arg(long, default_value = "unknown")]
    model: String,
    /// Optional task description
    #[arg(long)]
    task: Option<String>,
}

#[derive(Debug, Args)]
struct AgentList {
    /// Project name/slug
    #[arg(long, value_name = "PROJECT")]
    project: Option<String>,
}

#[derive(Debug, Args)]
struct AgentInfo {
    /// Agent id
    #[arg(value_name = "ID")]
    id: i64,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ImportanceArg {
    Low,
    Normal,
    High,
    Urgent,
}

impl From<ImportanceArg> for Importance {
    fn from(value: ImportanceArg) -> Self {
        match value {
            ImportanceArg::Low => Importance::Low,
            ImportanceArg::Normal => Importance::Normal,
            ImportanceArg::High => Importance::High,
            ImportanceArg::Urgent => Importance::Urgent,
        }
    }
}

#[derive(Debug, Args)]
struct SendCommand {
    /// Recipient agent name
    #[arg(value_name = "TO")]
    to: String,
    /// Subject line
    #[arg(value_name = "SUBJECT")]
    subject: String,
    /// Message body (or read from stdin)
    #[arg(long)]
    body: Option<String>,
    /// CC recipients (comma separated or repeated)
    #[arg(long, value_delimiter = ',', action = clap::ArgAction::Append)]
    cc: Vec<String>,
    /// BCC recipients (comma separated or repeated)
    #[arg(long, value_delimiter = ',', action = clap::ArgAction::Append)]
    bcc: Vec<String>,
    /// Importance level
    #[arg(long, value_enum)]
    importance: Option<ImportanceArg>,
    /// Require acknowledgement
    #[arg(long)]
    ack: bool,
    /// Optional thread id
    #[arg(long)]
    thread: Option<String>,
}

#[derive(Debug, Args)]
struct InboxCommand {
    /// Show only unread messages
    #[arg(long)]
    unread: bool,
    /// Maximum number of messages to return
    #[arg(long, default_value = "20")]
    limit: usize,
}

#[derive(Debug, Args)]
struct ReadCommand {
    /// Message id
    #[arg(value_name = "ID")]
    id: i64,
}

#[derive(Debug, Args)]
struct AckCommand {
    /// Message id
    #[arg(value_name = "ID")]
    id: i64,
}

#[derive(Debug, Args)]
struct SearchCommand {
    /// Query string
    #[arg(value_name = "QUERY")]
    query: String,
    /// Maximum number of matches to return
    #[arg(long, default_value = "20")]
    limit: usize,
}

#[derive(Debug, Args)]
struct ReserveCommand {
    /// Files to reserve
    #[arg(value_name = "FILE", num_args = 1..)]
    files: Vec<String>,
    /// Exclusive reservation
    #[arg(long, default_value_t = true)]
    exclusive: bool,
    /// Shared reservation
    #[arg(long)]
    shared: bool,
    /// Time-to-live in seconds (default: 1800)
    #[arg(long)]
    ttl: Option<u64>,
    /// Reason for the reservation
    #[arg(long)]
    reason: Option<String>,
}

#[derive(Debug, Args)]
struct ReleaseCommand {
    /// Reservation id
    #[arg(value_name = "ID")]
    reservation_id: Option<i64>,
    /// Release all reservations for the current agent
    #[arg(long)]
    all: bool,
}

#[derive(Debug, Args)]
struct ReservationsCommand {
    /// Show only reservations owned by the current agent
    #[arg(long)]
    mine: bool,
    /// Filter by path
    #[arg(long, value_name = "PATH")]
    file: Option<String>,
}

#[derive(Debug, Args)]
struct CheckCommand {
    /// Files to check
    #[arg(value_name = "FILE", num_args = 1..)]
    files: Vec<String>,
}

#[derive(Debug, Args)]
struct GcCommand {
    /// Retention window in days for messages
    #[arg(long)]
    retention_days: Option<u64>,
}

#[derive(Debug, Args)]
struct WatchCommand {
    /// Poll interval in seconds
    #[arg(long)]
    interval: Option<u64>,
    /// Run in daemon mode (updates daemon state and logs to file)
    #[arg(long)]
    daemon: bool,
}

#[derive(Debug, Subcommand)]
enum DaemonCommand {
    /// Start the daemon
    Start(DaemonStart),
    /// Stop the daemon
    Stop,
    /// Show daemon status
    Status,
}

#[derive(Debug, Args)]
struct DaemonStart {
    /// Poll interval in seconds
    #[arg(long)]
    interval: Option<u64>,
}

#[derive(Debug, Clone)]
struct RuntimeContext {
    common: CommonOpts,
    paths: AppPaths,
    config: AppConfig,
}

impl RuntimeContext {
    fn new(common: CommonOpts) -> Result<Self> {
        let paths = AppPaths::discover(common.config.clone())?;
        let config = AppConfig::load(&paths, common.dry_run)?;
        let paths = paths.apply_overrides(&config)?;
        let ctx = Self {
            common,
            paths,
            config,
        };
        ctx.ensure_directories()?;
        Ok(ctx)
    }

    fn init_logging(&self) -> Result<()> {
        if self.common.quiet {
            log::set_max_level(LevelFilter::Off);
            return Ok(());
        }

        let mut builder =
            env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));

        builder.filter_level(self.effective_log_level());

        let force_color = matches!(self.common.color, ColorOption::Always)
            || env::var_os("FORCE_COLOR").is_some();
        let disable_color = self.common.no_color
            || matches!(self.common.color, ColorOption::Never)
            || env::var_os("NO_COLOR").is_some()
            || (!force_color && !io::stderr().is_terminal());

        if disable_color {
            builder.write_style(WriteStyle::Never);
        } else if force_color {
            builder.write_style(WriteStyle::Always);
        } else {
            builder.write_style(WriteStyle::Auto);
        }

        if self.common.diagnostics {
            builder.format_timestamp_millis();
            builder.format_module_path(true);
            builder.format_target(true);
        }

        builder.try_init().or_else(|err| {
            if self.common.verbose > 0 {
                eprintln!("logger already initialized: {err}");
            }
            Ok(())
        })
    }

    fn effective_log_level(&self) -> LevelFilter {
        if self.common.trace {
            LevelFilter::Trace
        } else if self.common.debug {
            LevelFilter::Debug
        } else {
            match self.common.verbose {
                0 => LevelFilter::Info,
                1 => LevelFilter::Debug,
                _ => LevelFilter::Trace,
            }
        }
    }

    fn ensure_directories(&self) -> Result<()> {
        if self.common.dry_run {
            self.paths.log_dry_run();
            return Ok(());
        }
        self.paths.ensure_directories()
    }

    fn storage(&self) -> Result<Storage> {
        Storage::open(&self.paths.database_file())
    }
}

fn handle_run(ctx: &mut RuntimeContext, cmd: RunCommand) -> Result<()> {
    let effective = ctx.config.clone().with_profile_override(cmd.profile);
    let output = if ctx.common.json {
        serde_json::to_string_pretty(&effective).context("serializing run output to JSON")?
    } else if ctx.common.yaml {
        serde_yaml::to_string(&effective).context("serializing run output to YAML")?
    } else {
        format!(
            "Running task '{}' with profile '{}' (parallelism: {})",
            cmd.task,
            effective.profile,
            effective
                .runtime
                .parallelism
                .unwrap_or_else(default_parallelism)
        )
    };

    println!("{output}");
    Ok(())
}

fn handle_init(ctx: &RuntimeContext, cmd: InitCommand) -> Result<()> {
    if ctx.paths.config_file.exists() && !(cmd.force || ctx.common.assume_yes) {
        return Err(anyhow!(
            "config already exists at {} (use --force to overwrite)",
            ctx.paths.config_file.display()
        ));
    }

    if ctx.common.dry_run {
        info!(
            "dry-run: would write default config to {}",
            ctx.paths.config_file.display()
        );
        return Ok(());
    }

    mailz_core::paths::write_default_config(&ctx.paths.config_file)
}

fn handle_config(ctx: &RuntimeContext, command: ConfigCommand) -> Result<()> {
    match command {
        ConfigCommand::Show => {
            if ctx.common.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&ctx.config)
                        .context("serializing config to JSON")?
                );
            } else if ctx.common.yaml {
                println!(
                    "{}",
                    serde_yaml::to_string(&ctx.config).context("serializing config to YAML")?
                );
            } else {
                println!("{:#?}", ctx.config);
            }
            Ok(())
        }
        ConfigCommand::Path => {
            println!("{}", ctx.paths.config_file.display());
            Ok(())
        }
        ConfigCommand::Paths => {
            let cache_dir = default_cache_dir()?;
            if ctx.common.json {
                let paths = serde_json::json!({
                    "config": ctx.paths.config_file,
                    "data": ctx.paths.data_dir,
                    "state": ctx.paths.state_dir,
                    "cache": cache_dir,
                });
                println!(
                    "{}",
                    serde_json::to_string_pretty(&paths).context("serializing paths to JSON")?
                );
            } else if ctx.common.yaml {
                let paths = serde_json::json!({
                    "config": ctx.paths.config_file,
                    "data": ctx.paths.data_dir,
                    "state": ctx.paths.state_dir,
                    "cache": cache_dir,
                });
                println!(
                    "{}",
                    serde_yaml::to_string(&paths).context("serializing paths to YAML")?
                );
            } else {
                println!("config: {}", ctx.paths.config_file.display());
                println!("data:   {}", ctx.paths.data_dir.display());
                println!("state:  {}", ctx.paths.state_dir.display());
                println!("cache:  {}", cache_dir.display());
            }
            Ok(())
        }
        ConfigCommand::Schema => {
            println!("{}", include_str!("../../../examples/config.schema.json"));
            Ok(())
        }
        ConfigCommand::Reset => {
            if ctx.common.dry_run {
                info!(
                    "dry-run: would reset config at {}",
                    ctx.paths.config_file.display()
                );
                return Ok(());
            }
            mailz_core::paths::write_default_config(&ctx.paths.config_file)
        }
    }
}

fn handle_project(ctx: &RuntimeContext, command: ProjectCommand) -> Result<()> {
    let storage = ctx.storage()?;
    match command {
        ProjectCommand::List => {
            let projects = storage.list_projects()?;
            print_output(ctx, &projects, || {
                for project in &projects {
                    println!("{}\t{}", project.slug, project.path);
                }
            })
        }
        ProjectCommand::Create(cmd) => {
            let path = expand_str_path(&cmd.path)?;
            if ctx.common.dry_run {
                return print_output(
                    ctx,
                    &serde_json::json!({
                        "status": "dry-run",
                        "path": path,
                        "name": cmd.name,
                    }),
                    || {
                        println!("dry-run: would create project at {}", path.display());
                    },
                );
            }

            let project =
                storage.create_project(path.to_string_lossy().as_ref(), cmd.name.as_deref())?;
            print_output(ctx, &project, || {
                println!("created {} at {}", project.slug, project.path);
            })
        }
        ProjectCommand::Info(cmd) => {
            let project = storage
                .get_project_by_slug(&cmd.name)?
                .ok_or_else(|| anyhow!("project '{}' not found", cmd.name))?;
            print_output(ctx, &project, || {
                println!("{}\t{}", project.slug, project.path);
            })
        }
        ProjectCommand::Delete(cmd) => {
            if !ctx.common.assume_yes {
                return Err(anyhow!("refusing to delete without --yes"));
            }
            if ctx.common.dry_run {
                return print_output(
                    ctx,
                    &serde_json::json!({
                        "status": "dry-run",
                        "project": cmd.name,
                    }),
                    || {
                        println!("dry-run: would delete project {}", cmd.name);
                    },
                );
            }
            let deleted = storage.delete_project(&cmd.name)?;
            if ctx.common.json || ctx.common.yaml {
                print_output(ctx, &serde_json::json!({"deleted": deleted}), || {})
            } else if deleted {
                println!("deleted {}", cmd.name);
                Ok(())
            } else {
                Err(anyhow!("project '{}' not found", cmd.name))
            }
        }
    }
}

fn handle_agent(ctx: &RuntimeContext, command: AgentCommand) -> Result<()> {
    let storage = ctx.storage()?;
    match command {
        AgentCommand::Register(cmd) => {
            let project = resolve_project(&storage, &ctx.paths, cmd.project.as_deref())?;
            if ctx.common.dry_run {
                return print_output(
                    ctx,
                    &serde_json::json!({
                        "status": "dry-run",
                        "project": project.slug,
                        "name": cmd.name,
                    }),
                    || {
                        println!("dry-run: would register agent in {}", project.slug);
                    },
                );
            }
            let agent = storage.register_agent(
                project.id,
                &CreateAgent {
                    name: cmd.name,
                    program: cmd.program,
                    model: cmd.model,
                    task_description: cmd.task,
                },
            )?;

            let identity = AgentIdentity {
                project_slug: project.slug.clone(),
                project_id: project.id,
                agent_id: agent.id,
                agent_name: agent.name.clone(),
            };
            save_agent_identity(&ctx.paths, &identity)?;

            print_output(ctx, &agent, || {
                println!("registered {} in {}", agent.name, project.slug);
            })
        }
        AgentCommand::List(cmd) => {
            let project = resolve_project(&storage, &ctx.paths, cmd.project.as_deref())?;
            let agents = storage.list_agents(project.id)?;
            print_output(ctx, &agents, || {
                for agent in &agents {
                    println!("{}\t{}\t{}", agent.id, agent.name, agent.program);
                }
            })
        }
        AgentCommand::Info(cmd) => {
            let agent = storage
                .get_agent_by_id(cmd.id)?
                .ok_or_else(|| anyhow!("agent {} not found", cmd.id))?;
            print_output(ctx, &agent, || {
                println!("{}\t{}\t{}", agent.id, agent.name, agent.program);
            })
        }
        AgentCommand::Whoami => match load_agent_identity(&ctx.paths)? {
            Some(identity) => print_output(ctx, &identity, || {
                println!(
                    "{} in {} (id {})",
                    identity.agent_name, identity.project_slug, identity.agent_id
                );
            }),
            None => {
                if ctx.common.json || ctx.common.yaml {
                    print_output(ctx, &Option::<AgentIdentity>::None, || {})
                } else {
                    println!("no agent identity set");
                    Ok(())
                }
            }
        },
    }
}

fn handle_send(ctx: &RuntimeContext, cmd: SendCommand) -> Result<()> {
    let storage = ctx.storage()?;
    let identity = require_agent_identity(&ctx.paths)?;

    let body = match cmd.body {
        Some(body) => body,
        None => read_body_from_stdin()?,
    };

    if body.trim().is_empty() {
        return Err(anyhow!("message body is empty"));
    }

    if ctx.common.dry_run {
        return print_output(
            ctx,
            &serde_json::json!({
                "status": "dry-run",
                "to": cmd.to,
                "subject": cmd.subject,
            }),
            || {
                println!("dry-run: would send to {}", cmd.to);
            },
        );
    }

    let message = storage.send_message(
        identity.project_id,
        &SendMessage {
            sender_name: identity.agent_name,
            to: vec![cmd.to],
            cc: if cmd.cc.is_empty() {
                None
            } else {
                Some(cmd.cc)
            },
            bcc: if cmd.bcc.is_empty() {
                None
            } else {
                Some(cmd.bcc)
            },
            subject: cmd.subject,
            body,
            importance: cmd.importance.map(Importance::from),
            ack_required: Some(cmd.ack),
            thread_id: cmd.thread,
        },
    )?;

    print_output(ctx, &message, || {
        println!("sent {}", message.id);
    })
}

fn handle_inbox(ctx: &RuntimeContext, cmd: InboxCommand) -> Result<()> {
    let storage = ctx.storage()?;
    let identity = require_agent_identity(&ctx.paths)?;

    let messages = storage.fetch_inbox(
        identity.project_id,
        &identity.agent_name,
        cmd.limit,
        0,
        cmd.unread,
    )?;

    print_output(ctx, &messages, || {
        for message in &messages {
            let flag = if message.read_at.is_none() { "*" } else { " " };
            println!(
                "{flag} {}\t{}\t{}",
                message.id, message.sender, message.subject
            );
        }
    })
}

fn handle_read(ctx: &RuntimeContext, cmd: ReadCommand) -> Result<()> {
    let storage = ctx.storage()?;
    let identity = require_agent_identity(&ctx.paths)?;
    let message = storage
        .get_message_view(identity.project_id, identity.agent_id, cmd.id)?
        .ok_or_else(|| anyhow!("message {} not found", cmd.id))?;

    if !ctx.common.dry_run {
        storage.mark_read(identity.agent_id, cmd.id)?;
    }

    print_output(ctx, &message, || {
        println!("From: {}", message.sender);
        println!("Subject: {}", message.subject);
        println!("\n{}", message.body);
    })
}

fn handle_ack(ctx: &RuntimeContext, cmd: AckCommand) -> Result<()> {
    let storage = ctx.storage()?;
    let identity = require_agent_identity(&ctx.paths)?;
    if !ctx.common.dry_run {
        storage.acknowledge(identity.agent_id, cmd.id)?;
    }
    let message = storage
        .get_message_view(identity.project_id, identity.agent_id, cmd.id)?
        .ok_or_else(|| anyhow!("message {} not found", cmd.id))?;

    print_output(ctx, &message, || {
        println!("acknowledged {}", cmd.id);
    })
}

fn handle_search(ctx: &RuntimeContext, cmd: SearchCommand) -> Result<()> {
    let storage = ctx.storage()?;
    let identity = require_agent_identity(&ctx.paths)?;
    let results = storage.search_messages_with_filters(
        identity.project_id,
        &cmd.query,
        cmd.limit,
        0,
        Some(identity.agent_id),
        false,
        None,
        None,
    )?;

    print_output(ctx, &results, || {
        for message in &results {
            println!("{}\t{}\t{}", message.id, message.sender, message.subject);
        }
    })
}

fn handle_reserve(ctx: &RuntimeContext, cmd: ReserveCommand) -> Result<()> {
    let storage = ctx.storage()?;
    let identity = require_agent_identity(&ctx.paths)?;
    let exclusive = if cmd.shared { false } else { cmd.exclusive };

    if ctx.common.dry_run {
        return print_output(
            ctx,
            &serde_json::json!({
                "status": "dry-run",
                "files": cmd.files,
            }),
            || {
                println!("dry-run: would reserve {} files", cmd.files.len());
            },
        );
    }

    let result = storage.create_file_reservations(
        identity.project_id,
        &CreateFileReservation {
            agent_name: identity.agent_name,
            paths: cmd.files,
            ttl_seconds: cmd.ttl,
            exclusive: Some(exclusive),
            reason: cmd.reason,
        },
    )?;

    print_output(ctx, &result, || {
        for res in &result.granted {
            println!("reserved {}", res.path_pattern);
        }
        if !result.conflicts.is_empty() {
            println!("conflicts:");
            for conflict in &result.conflicts {
                println!("- {} (agent {})", conflict.path_pattern, conflict.agent_id);
            }
        }
    })
}

fn handle_release(ctx: &RuntimeContext, cmd: ReleaseCommand) -> Result<()> {
    let storage = ctx.storage()?;
    let identity = require_agent_identity(&ctx.paths)?;

    if cmd.reservation_id.is_none() && !cmd.all {
        return Err(anyhow!("provide a reservation id or --all"));
    }

    if ctx.common.dry_run {
        return print_output(
            ctx,
            &serde_json::json!({
                "status": "dry-run",
                "reservation_id": cmd.reservation_id,
                "all": cmd.all,
            }),
            || {
                println!("dry-run: would release reservations");
            },
        );
    }

    if let Some(id) = cmd.reservation_id {
        let released =
            storage.release_reservation_by_id(identity.project_id, &identity.agent_name, id)?;
        print_output(ctx, &serde_json::json!({"released": released}), || {
            if released {
                println!("released {id}");
            } else {
                println!("reservation {id} not found");
            }
        })
    } else {
        let count =
            storage.release_reservations(identity.project_id, &identity.agent_name, None)?;
        print_output(ctx, &serde_json::json!({"released": count}), || {
            println!("released {count} reservations");
        })
    }
}

fn handle_reservations(ctx: &RuntimeContext, cmd: ReservationsCommand) -> Result<()> {
    let storage = ctx.storage()?;
    let identity = require_agent_identity(&ctx.paths)?;
    let agent_filter = if cmd.mine {
        Some(identity.agent_id)
    } else {
        None
    };
    let reservations =
        storage.list_active_reservations(identity.project_id, agent_filter, cmd.file.as_deref())?;

    print_output(ctx, &reservations, || {
        for res in &reservations {
            println!(
                "{}\t{}\t{}\t{}",
                res.id, res.agent_id, res.path_pattern, res.expires_at
            );
        }
    })
}

fn handle_check(ctx: &RuntimeContext, cmd: CheckCommand) -> Result<()> {
    let storage = ctx.storage()?;
    let identity = require_agent_identity(&ctx.paths)?;
    let conflicts = storage.check_reservations(identity.project_id, &cmd.files)?;

    print_output(ctx, &conflicts, || {
        if conflicts.is_empty() {
            println!("no conflicts");
        } else {
            for conflict in &conflicts {
                println!(
                    "{}\t{}\t{}",
                    conflict.path_pattern, conflict.agent_id, conflict.expires_at
                );
            }
        }
    })
}

fn handle_gc(ctx: &RuntimeContext, cmd: GcCommand) -> Result<()> {
    let storage = ctx.storage()?;
    let retention = cmd
        .retention_days
        .unwrap_or(ctx.config.maintenance.message_retention_days);

    if ctx.common.dry_run {
        return print_output(
            ctx,
            &serde_json::json!({ "status": "dry-run", "retention_days": retention }),
            || {
                println!("dry-run: would run gc with retention {retention} days");
            },
        );
    }

    let summary = storage.run_gc(retention)?;
    print_output(ctx, &summary, || {
        println!("expired reservations: {}", summary.expired_reservations);
        println!("deleted messages: {}", summary.deleted_messages);
    })
}

fn handle_watch(ctx: &RuntimeContext, cmd: WatchCommand) -> Result<()> {
    let storage = ctx.storage()?;
    let identity = require_agent_identity(&ctx.paths)?;
    let interval = cmd
        .interval
        .unwrap_or(ctx.config.watch.poll_interval_seconds)
        .max(1);

    let mut output = WatchOutput::new(cmd.daemon, &ctx.paths)?;
    let mut last_seen = Utc::now();
    let mut last_gc = Instant::now();
    let mut last_tick = Instant::now();

    let started_at = Utc::now();
    if cmd.daemon {
        if ctx.paths.daemon_stop_file().exists() {
            fs::remove_file(ctx.paths.daemon_stop_file())?;
        }
        let state = DaemonState {
            pid: std::process::id(),
            project_slug: identity.project_slug.clone(),
            agent_name: identity.agent_name.clone(),
            started_at,
            last_heartbeat: started_at,
            poll_interval_seconds: interval,
        };
        save_daemon_state(&ctx.paths, &state)?;
    }

    loop {
        if cmd.daemon && ctx.paths.daemon_stop_file().exists() {
            output.write_line("daemon stop requested")?;
            clear_daemon_state(&ctx.paths)?;
            fs::remove_file(ctx.paths.daemon_stop_file())?;
            break;
        }

        if last_tick.elapsed() >= Duration::from_secs(interval) {
            last_tick = Instant::now();
            let mut messages = storage.fetch_inbox_for_agent(
                identity.project_id,
                identity.agent_id,
                50,
                0,
                false,
                Some(last_seen),
                None,
            )?;
            messages.sort_by_key(|m| m.created_at);
            for message in &messages {
                output.write_line(&format!(
                    "[{}] New from {}: {}",
                    message.created_at, message.sender, message.subject
                ))?;
            }
            if let Some(latest) = messages.last() {
                last_seen = latest.created_at + chrono::Duration::milliseconds(1);
            }

            if cmd.daemon {
                let now = Utc::now();
                let state = DaemonState {
                    pid: std::process::id(),
                    project_slug: identity.project_slug.clone(),
                    agent_name: identity.agent_name.clone(),
                    started_at,
                    last_heartbeat: now,
                    poll_interval_seconds: interval,
                };
                save_daemon_state(&ctx.paths, &state)?;
            }
        }

        let gc_interval = ctx.config.maintenance.gc_interval_seconds.max(1);
        if last_gc.elapsed() >= Duration::from_secs(gc_interval) {
            last_gc = Instant::now();
            let summary = storage.run_gc(ctx.config.maintenance.message_retention_days)?;
            if summary.expired_reservations > 0 || summary.deleted_messages > 0 {
                output.write_line(&format!(
                    "gc: expired {}, deleted {}",
                    summary.expired_reservations, summary.deleted_messages
                ))?;
            }
        }

        std::thread::sleep(Duration::from_millis(200));
    }

    Ok(())
}

fn handle_daemon(ctx: &RuntimeContext, command: DaemonCommand) -> Result<()> {
    match command {
        DaemonCommand::Start(cmd) => handle_daemon_start(ctx, cmd),
        DaemonCommand::Stop => handle_daemon_stop(ctx),
        DaemonCommand::Status => handle_daemon_status(ctx),
    }
}

fn handle_daemon_start(ctx: &RuntimeContext, cmd: DaemonStart) -> Result<()> {
    let status = daemon_status(&ctx.paths)?;
    if status.running {
        return Err(anyhow!("daemon already running"));
    }
    if status.state.is_some() {
        clear_daemon_state(&ctx.paths)?;
    }
    if ctx.common.dry_run {
        println!("dry-run: would start daemon");
        return Ok(());
    }

    if ctx.paths.daemon_stop_file().exists() {
        fs::remove_file(ctx.paths.daemon_stop_file())?;
    }

    let exe = env::current_exe()?;
    let mut command = std::process::Command::new(exe);
    command.arg("watch").arg("--daemon");
    if let Some(interval) = cmd.interval {
        command.arg("--interval").arg(interval.to_string());
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.spawn().context("starting daemon")?;

    println!("daemon started");
    Ok(())
}

fn handle_daemon_stop(ctx: &RuntimeContext) -> Result<()> {
    let status = daemon_status(&ctx.paths)?;
    if !status.running {
        println!("daemon not running");
        return Ok(());
    }
    if ctx.common.dry_run {
        println!("dry-run: would stop daemon");
        return Ok(());
    }
    fs::write(ctx.paths.daemon_stop_file(), "stop")?;
    let timeout = Duration::from_secs(5);
    let start = Instant::now();
    while start.elapsed() < timeout {
        if daemon_status(&ctx.paths)?.running {
            std::thread::sleep(Duration::from_millis(200));
        } else {
            println!("daemon stopped");
            return Ok(());
        }
    }
    println!("daemon stop requested; waiting for shutdown");
    Ok(())
}

fn handle_daemon_status(ctx: &RuntimeContext) -> Result<()> {
    let status = daemon_status(&ctx.paths)?;
    if let Some(state) = status.state {
        if status.running {
            println!(
                "running (pid {}, agent {}, project {})",
                state.pid, state.agent_name, state.project_slug
            );
        } else {
            println!(
                "stale state (pid {}, last heartbeat {})",
                state.pid, state.last_heartbeat
            );
        }
    } else {
        println!("not running");
    }
    Ok(())
}

struct DaemonStatus {
    running: bool,
    state: Option<DaemonState>,
}

fn daemon_status(paths: &AppPaths) -> Result<DaemonStatus> {
    let state = load_daemon_state(paths)?;
    let Some(state) = state else {
        return Ok(DaemonStatus {
            running: false,
            state: None,
        });
    };
    let now = Utc::now();
    let threshold = chrono::Duration::seconds((state.poll_interval_seconds * 2) as i64);
    let running = now - state.last_heartbeat <= threshold;
    Ok(DaemonStatus {
        running,
        state: Some(state),
    })
}

struct WatchOutput {
    writer: Box<dyn Write + Send>,
}

impl WatchOutput {
    fn new(daemon: bool, paths: &AppPaths) -> Result<Self> {
        if daemon {
            let file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(paths.daemon_log_file())?;
            Ok(Self {
                writer: Box::new(io::BufWriter::new(file)),
            })
        } else {
            Ok(Self {
                writer: Box::new(io::stdout()),
            })
        }
    }

    fn write_line(&mut self, line: &str) -> Result<()> {
        writeln!(self.writer, "{line}")?;
        self.writer.flush()?;
        Ok(())
    }
}

fn handle_completions(shell: Shell) -> Result<()> {
    let mut cmd = Cli::command();
    clap_complete::generate(shell, &mut cmd, APP_NAME, &mut io::stdout());
    Ok(())
}

fn print_output<T: Serialize>(ctx: &RuntimeContext, value: &T, text: impl FnOnce()) -> Result<()> {
    if ctx.common.json {
        println!(
            "{}",
            serde_json::to_string_pretty(value).context("serializing output to JSON")?
        );
    } else if ctx.common.yaml {
        println!(
            "{}",
            serde_yaml::to_string(value).context("serializing output to YAML")?
        );
    } else {
        text();
    }
    Ok(())
}

fn require_agent_identity(paths: &AppPaths) -> Result<AgentIdentity> {
    load_agent_identity(paths)?
        .ok_or_else(|| anyhow!("no agent identity set (run `mailz agent register`)"))
}

fn resolve_project(
    storage: &Storage,
    paths: &AppPaths,
    slug: Option<&str>,
) -> Result<mailz_core::Project> {
    if let Some(slug) = slug {
        return storage
            .get_project_by_slug(slug)?
            .ok_or_else(|| anyhow!("project '{}' not found", slug));
    }

    if let Some(identity) = load_agent_identity(paths)? {
        return storage
            .get_project_by_slug(&identity.project_slug)?
            .ok_or_else(|| anyhow!("project '{}' not found", identity.project_slug));
    }

    Err(anyhow!("project not specified"))
}

fn read_body_from_stdin() -> Result<String> {
    if io::stdin().is_terminal() {
        return Ok(String::new());
    }

    let mut buffer = String::new();
    io::stdin()
        .read_to_string(&mut buffer)
        .context("reading message body from stdin")?;
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_importance_mapping() {
        assert_eq!(Importance::Low, ImportanceArg::Low.into());
        assert_eq!(Importance::Normal, ImportanceArg::Normal.into());
        assert_eq!(Importance::High, ImportanceArg::High.into());
        assert_eq!(Importance::Urgent, ImportanceArg::Urgent.into());
    }
}
