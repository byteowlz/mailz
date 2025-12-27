use std::collections::HashMap;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use chrono::Utc;
use clap::{Args, Parser};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
};

use mailz_core::{
    Agent, AgentIdentity, AppConfig, AppPaths, DraftMessage, FileReservation, InboxSummary,
    MessageView, Project, Storage, clear_draft, load_agent_identity, load_draft, save_draft,
};

fn main() {
    if let Err(err) = try_main() {
        let _ = writeln!(io::stderr(), "{err:?}");
        std::process::exit(1);
    }
}

fn try_main() -> Result<()> {
    let cli = Cli::parse();
    let paths = AppPaths::discover(cli.common.config)?;
    let config = AppConfig::load(&paths, false)?;
    paths.ensure_directories()?;
    let storage = Storage::open(&paths.database_file())?;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(config, paths, storage)?;
    let result = run_app(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    result
}

#[derive(Debug, Parser)]
#[command(author, version, about = "TUI interface for mailz")]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Inbox,
    Compose,
    Reservations,
    Directory,
    MessageDetail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComposeField {
    To,
    Cc,
    Bcc,
    Subject,
    Body,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Search,
    Reserve,
    ProjectFilter,
    ProjectCreate,
    AgentRegister,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirectoryTab {
    Projects,
    Agents,
}

struct InboxState {
    folders: Vec<&'static str>,
    selected_folder: usize,
    messages: Vec<MessageView>,
    selected_message: usize,
    search_query: Option<String>,
}

struct ComposeState {
    active: ComposeField,
    to: String,
    cc: String,
    bcc: String,
    subject: String,
    body: String,
}

struct ReservationsState {
    reservations: Vec<FileReservation>,
    selected: usize,
    project_filter: Option<String>,
}

struct DirectoryState {
    tab: DirectoryTab,
    projects: Vec<Project>,
    selected_project: usize,
    agents: Vec<Agent>,
    summaries: HashMap<String, InboxSummary>,
    agent_counts: HashMap<i64, usize>,
    selected_agent: usize,
}

struct App {
    paths: AppPaths,
    storage: Storage,
    identity: Option<AgentIdentity>,
    view: View,
    inbox: InboxState,
    compose: ComposeState,
    reservations: ReservationsState,
    directory: DirectoryState,
    input_mode: Option<InputMode>,
    input_buffer: String,
    status_message: String,
    agent_names: Vec<String>,
    focused_message: Option<MessageView>,
    poll_interval: Duration,
    last_poll: Instant,
    unread_count: usize,
    new_message_flag: bool,
}

impl App {
    fn new(config: AppConfig, paths: AppPaths, storage: Storage) -> Result<Self> {
        let identity = load_agent_identity(&paths)?;
        let poll_interval = Duration::from_secs(config.tui.poll_interval_seconds.max(1));
        let mut app = Self {
            paths,
            storage,
            identity,
            view: View::Inbox,
            inbox: InboxState {
                folders: vec!["Inbox", "Sent", "All"],
                selected_folder: 0,
                messages: Vec::new(),
                selected_message: 0,
                search_query: None,
            },
            compose: ComposeState {
                active: ComposeField::To,
                to: String::new(),
                cc: String::new(),
                bcc: String::new(),
                subject: String::new(),
                body: String::new(),
            },
            reservations: ReservationsState {
                reservations: Vec::new(),
                selected: 0,
                project_filter: None,
            },
            directory: DirectoryState {
                tab: DirectoryTab::Projects,
                projects: Vec::new(),
                selected_project: 0,
                agents: Vec::new(),
                summaries: HashMap::new(),
                agent_counts: HashMap::new(),
                selected_agent: 0,
            },
            input_mode: None,
            input_buffer: String::new(),
            status_message: String::new(),
            agent_names: Vec::new(),
            focused_message: None,
            poll_interval,
            last_poll: Instant::now(),
            unread_count: 0,
            new_message_flag: false,
        };

        app.status_message = if app.identity.is_none() {
            "Run `mailz agent register` to set identity".to_string()
        } else {
            "Press c to compose, r to toggle read, a to acknowledge".to_string()
        };

        app.refresh_agents()?;
        app.load_draft_if_any()?;
        app.refresh_inbox()?;
        app.refresh_reservations()?;
        app.refresh_directory()?;

        Ok(app)
    }

    fn refresh_agents(&mut self) -> Result<()> {
        let Some(identity) = &self.identity else {
            return Ok(());
        };
        let agents = self.storage.list_agents(identity.project_id)?;
        self.agent_names = agents.into_iter().map(|agent| agent.name).collect();
        Ok(())
    }

    fn refresh_inbox(&mut self) -> Result<()> {
        let Some(identity) = &self.identity else {
            self.inbox.messages.clear();
            return Ok(());
        };
        let messages = if let Some(query) = &self.inbox.search_query {
            self.storage.search_messages_with_filters(
                identity.project_id,
                query,
                50,
                0,
                Some(identity.agent_id),
                false,
                None,
                None,
            )?
        } else {
            self.storage.fetch_inbox_for_agent(
                identity.project_id,
                identity.agent_id,
                50,
                0,
                false,
                None,
                None,
            )?
        };
        let unread = messages.iter().filter(|m| m.read_at.is_none()).count();
        if unread > self.unread_count {
            self.new_message_flag = true;
            self.status_message = "New messages arrived".to_string();
        }
        self.unread_count = unread;
        self.inbox.messages = messages;
        if self.inbox.selected_message >= self.inbox.messages.len() {
            self.inbox.selected_message = 0;
        }
        Ok(())
    }

    fn refresh_reservations(&mut self) -> Result<()> {
        let Some(identity) = &self.identity else {
            self.reservations.reservations.clear();
            return Ok(());
        };
        let project_id = if let Some(slug) = &self.reservations.project_filter {
            self.storage
                .get_project_by_slug(slug)?
                .map(|project| project.id)
                .unwrap_or(identity.project_id)
        } else {
            identity.project_id
        };
        let reservations = self
            .storage
            .list_active_reservations(project_id, None, None)?;
        self.reservations.reservations = reservations;
        if self.reservations.selected >= self.reservations.reservations.len() {
            self.reservations.selected = 0;
        }
        Ok(())
    }

    fn refresh_directory(&mut self) -> Result<()> {
        let Some(identity) = &self.identity else {
            self.directory.projects.clear();
            self.directory.agents.clear();
            self.directory.summaries.clear();
            self.directory.agent_counts.clear();
            return Ok(());
        };

        let projects = self.storage.list_projects()?;
        let agent_counts = self.storage.agent_counts_by_project()?;
        self.directory.projects = projects;
        self.directory.agent_counts = agent_counts;
        if self.directory.selected_project >= self.directory.projects.len() {
            self.directory.selected_project = 0;
        }

        let project_id = self
            .directory
            .projects
            .get(self.directory.selected_project)
            .map(|project| project.id)
            .unwrap_or(identity.project_id);

        let agents = self.storage.list_agents(project_id)?;
        let summaries = self.storage.list_inbox_summaries(project_id)?;
        self.directory.agents = agents;
        self.directory.summaries = summaries
            .into_iter()
            .map(|summary| (summary.agent.clone(), summary))
            .collect();
        if self.directory.selected_agent >= self.directory.agents.len() {
            self.directory.selected_agent = 0;
        }

        Ok(())
    }

    fn load_draft_if_any(&mut self) -> Result<()> {
        let Some(draft) = load_draft(&self.paths)? else {
            return Ok(());
        };
        self.compose.to = join_recipients(&draft.to);
        self.compose.cc = join_recipients(&draft.cc);
        self.compose.bcc = join_recipients(&draft.bcc);
        self.compose.subject = draft.subject;
        self.compose.body = draft.body;
        Ok(())
    }

    fn save_draft(&self) -> Result<()> {
        let draft = DraftMessage {
            to: parse_recipients(&self.compose.to),
            cc: parse_recipients(&self.compose.cc),
            bcc: parse_recipients(&self.compose.bcc),
            subject: self.compose.subject.clone(),
            body: self.compose.body.clone(),
        };
        save_draft(&self.paths, &draft)
    }

    fn clear_compose(&mut self) -> Result<()> {
        self.compose.to.clear();
        self.compose.cc.clear();
        self.compose.bcc.clear();
        self.compose.subject.clear();
        self.compose.body.clear();
        clear_draft(&self.paths)?;
        Ok(())
    }

    fn selected_message(&self) -> Option<&MessageView> {
        self.inbox.messages.get(self.inbox.selected_message)
    }

    fn selected_reservation(&self) -> Option<&FileReservation> {
        self.reservations
            .reservations
            .get(self.reservations.selected)
    }

    fn update_selected_message(&mut self) -> Result<()> {
        let Some(identity) = &self.identity else {
            return Ok(());
        };
        let Some(message) = self.selected_message() else {
            return Ok(());
        };
        let updated =
            self.storage
                .get_message_view(identity.project_id, identity.agent_id, message.id)?;
        if let Some(updated) = updated {
            self.inbox.messages[self.inbox.selected_message] = updated;
        }
        Ok(())
    }

    fn poll_if_due(&mut self) -> Result<()> {
        if self.last_poll.elapsed() < self.poll_interval {
            return Ok(());
        }
        self.last_poll = Instant::now();
        self.refresh_inbox()?;
        self.refresh_reservations()?;
        self.refresh_directory()?;
        Ok(())
    }
}

fn run_app<B: ratatui::backend::Backend>(terminal: &mut Terminal<B>, app: &mut App) -> Result<()> {
    loop {
        terminal.draw(|f| ui(f, app))?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }

                if key.code == KeyCode::Char('q') {
                    if matches!(app.input_mode, Some(_)) {
                        app.input_mode = None;
                        app.input_buffer.clear();
                        continue;
                    }
                    return Ok(());
                }

                if app.input_mode.is_some() {
                    handle_input_mode(app, key)?;
                    continue;
                }

                match app.view {
                    View::Inbox => handle_inbox_keys(app, key)?,
                    View::Compose => handle_compose_keys(app, key)?,
                    View::Reservations => handle_reservations_keys(app, key)?,
                    View::Directory => handle_directory_keys(app, key)?,
                    View::MessageDetail => handle_message_detail_keys(app, key)?,
                }
            }
        }

        app.poll_if_due()?;
    }
}

fn handle_input_mode(app: &mut App, key: KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Esc => {
            app.input_mode = None;
            app.input_buffer.clear();
        }
        KeyCode::Enter => {
            if let Some(mode) = app.input_mode.take() {
                let input = app.input_buffer.trim().to_string();
                app.input_buffer.clear();
                match mode {
                    InputMode::Search => {
                        if input.is_empty() {
                            app.inbox.search_query = None;
                        } else {
                            app.inbox.search_query = Some(input);
                        }
                        app.refresh_inbox()?;
                    }
                    InputMode::Reserve => {
                        let Some(identity) = &app.identity else {
                            app.status_message = "No agent identity".to_string();
                            return Ok(());
                        };
                        let files = parse_recipients(&input);
                        if files.is_empty() {
                            app.status_message = "No files provided".to_string();
                            return Ok(());
                        }
                        app.storage.create_file_reservations(
                            identity.project_id,
                            &mailz_core::CreateFileReservation {
                                agent_name: identity.agent_name.clone(),
                                paths: files,
                                ttl_seconds: None,
                                exclusive: Some(true),
                                reason: None,
                            },
                        )?;
                        app.refresh_reservations()?;
                        app.status_message = "Reservation created".to_string();
                    }
                    InputMode::ProjectCreate => {
                        let Some(identity) = &app.identity else {
                            app.status_message = "No agent identity".to_string();
                            return Ok(());
                        };
                        let agent_name = identity.agent_name.clone();
                        let mut parts = input.split_whitespace();
                        let Some(path) = parts.next() else {
                            app.status_message = "Project path required".to_string();
                            return Ok(());
                        };
                        let name = parts.next().map(|value| value.to_string());
                        app.storage.create_project(path, name.as_deref())?;
                        app.refresh_directory()?;
                        app.status_message = format!("Project created by {agent_name}");
                    }
                    InputMode::AgentRegister => {
                        let Some(identity) = &app.identity else {
                            app.status_message = "No agent identity".to_string();
                            return Ok(());
                        };
                        let project_id = app
                            .directory
                            .projects
                            .get(app.directory.selected_project)
                            .map(|project| project.id)
                            .unwrap_or(identity.project_id);
                        let name = if input.is_empty() { None } else { Some(input) };
                        app.storage.register_agent(
                            project_id,
                            &mailz_core::CreateAgent {
                                name,
                                program: "mailz-tui".to_string(),
                                model: "unknown".to_string(),
                                task_description: None,
                            },
                        )?;
                        app.refresh_directory()?;
                        app.refresh_agents()?;
                        app.status_message = "Agent registered".to_string();
                    }
                    InputMode::ProjectFilter => {
                        if input.is_empty() {
                            app.reservations.project_filter = None;
                        } else {
                            app.reservations.project_filter = Some(input);
                        }
                        app.refresh_reservations()?;
                    }
                }
            }
        }
        KeyCode::Backspace => {
            app.input_buffer.pop();
        }
        KeyCode::Char(ch) => {
            app.input_buffer.push(ch);
        }
        _ => {}
    }
    Ok(())
}

fn handle_inbox_keys(app: &mut App, key: KeyEvent) -> Result<()> {
    app.new_message_flag = false;
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => {
            if !app.inbox.messages.is_empty() {
                app.inbox.selected_message =
                    (app.inbox.selected_message + 1) % app.inbox.messages.len();
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if !app.inbox.messages.is_empty() {
                app.inbox.selected_message = app
                    .inbox
                    .selected_message
                    .checked_sub(1)
                    .unwrap_or(app.inbox.messages.len() - 1);
            }
        }
        KeyCode::Char('c') => {
            app.view = View::Compose;
        }
        KeyCode::Char('g') => {
            app.view = View::Directory;
        }
        KeyCode::Char('r') => {
            let Some(identity) = &app.identity else {
                return Ok(());
            };
            let Some(message) = app.selected_message() else {
                return Ok(());
            };
            if message.read_at.is_none() {
                app.storage.mark_read(identity.agent_id, message.id)?;
            } else {
                app.storage.mark_unread(identity.agent_id, message.id)?;
            }
            app.update_selected_message()?;
        }
        KeyCode::Char('a') => {
            let Some(identity) = &app.identity else {
                return Ok(());
            };
            let Some(message) = app.selected_message() else {
                return Ok(());
            };
            app.storage.acknowledge(identity.agent_id, message.id)?;
            app.update_selected_message()?;
        }
        KeyCode::Char('/') => {
            app.input_mode = Some(InputMode::Search);
            app.input_buffer.clear();
        }
        KeyCode::Enter => {
            if let Some(message) = app.selected_message() {
                app.focused_message = Some(message.clone());
                app.view = View::MessageDetail;
            }
        }
        KeyCode::Char('t') => {
            app.view = View::Reservations;
        }
        _ => {}
    }
    Ok(())
}

fn handle_message_detail_keys(app: &mut App, key: KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Esc | KeyCode::Enter => {
            app.view = View::Inbox;
            app.focused_message = None;
            app.new_message_flag = false;
        }
        _ => {}
    }
    Ok(())
}

fn handle_compose_keys(app: &mut App, key: KeyEvent) -> Result<()> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
        send_compose(app)?;
        return Ok(());
    }

    match key.code {
        KeyCode::Esc => {
            app.view = View::Inbox;
            app.clear_compose()?;
        }
        KeyCode::Tab => {
            if matches!(
                app.compose.active,
                ComposeField::To | ComposeField::Cc | ComposeField::Bcc
            ) {
                let applied = complete_agent_name(app);
                if applied {
                    app.save_draft()?;
                    return Ok(());
                }
            }
            app.compose.active = next_compose_field(app.compose.active);
        }
        KeyCode::BackTab => {
            app.compose.active = previous_compose_field(app.compose.active);
        }
        KeyCode::Enter => {
            if app.compose.active == ComposeField::Body {
                app.compose.body.push('\n');
            } else {
                app.compose.active = next_compose_field(app.compose.active);
            }
            app.save_draft()?;
        }
        KeyCode::Backspace => {
            match app.compose.active {
                ComposeField::To => {
                    app.compose.to.pop();
                }
                ComposeField::Cc => {
                    app.compose.cc.pop();
                }
                ComposeField::Bcc => {
                    app.compose.bcc.pop();
                }
                ComposeField::Subject => {
                    app.compose.subject.pop();
                }
                ComposeField::Body => {
                    app.compose.body.pop();
                }
            }
            app.save_draft()?;
        }
        KeyCode::Char(ch) => {
            match app.compose.active {
                ComposeField::To => app.compose.to.push(ch),
                ComposeField::Cc => app.compose.cc.push(ch),
                ComposeField::Bcc => app.compose.bcc.push(ch),
                ComposeField::Subject => app.compose.subject.push(ch),
                ComposeField::Body => app.compose.body.push(ch),
            }
            app.save_draft()?;
        }
        _ => {}
    }

    Ok(())
}

fn handle_reservations_keys(app: &mut App, key: KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => {
            if !app.reservations.reservations.is_empty() {
                app.reservations.selected =
                    (app.reservations.selected + 1) % app.reservations.reservations.len();
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if !app.reservations.reservations.is_empty() {
                app.reservations.selected = app
                    .reservations
                    .selected
                    .checked_sub(1)
                    .unwrap_or(app.reservations.reservations.len() - 1);
            }
        }
        KeyCode::Char('n') => {
            app.input_mode = Some(InputMode::Reserve);
            app.input_buffer.clear();
        }
        KeyCode::Char('d') => {
            let Some(identity) = &app.identity else {
                return Ok(());
            };
            let Some(reservation) = app.selected_reservation() else {
                return Ok(());
            };
            app.storage.release_reservation_by_id(
                identity.project_id,
                &identity.agent_name,
                reservation.id,
            )?;
            app.refresh_reservations()?;
        }
        KeyCode::Char('p') => {
            app.input_mode = Some(InputMode::ProjectFilter);
            app.input_buffer.clear();
        }
        KeyCode::Char('i') => {
            app.view = View::Inbox;
            app.new_message_flag = false;
        }
        KeyCode::Char('g') => {
            app.view = View::Directory;
        }
        _ => {}
    }
    Ok(())
}

fn handle_directory_keys(app: &mut App, key: KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Tab => {
            app.directory.tab = match app.directory.tab {
                DirectoryTab::Projects => DirectoryTab::Agents,
                DirectoryTab::Agents => DirectoryTab::Projects,
            };
        }
        KeyCode::Char('j') | KeyCode::Down => match app.directory.tab {
            DirectoryTab::Projects => {
                if !app.directory.projects.is_empty() {
                    app.directory.selected_project =
                        (app.directory.selected_project + 1) % app.directory.projects.len();
                    app.refresh_directory()?;
                }
            }
            DirectoryTab::Agents => {
                if !app.directory.agents.is_empty() {
                    app.directory.selected_agent =
                        (app.directory.selected_agent + 1) % app.directory.agents.len();
                }
            }
        },
        KeyCode::Char('k') | KeyCode::Up => match app.directory.tab {
            DirectoryTab::Projects => {
                if !app.directory.projects.is_empty() {
                    app.directory.selected_project = app
                        .directory
                        .selected_project
                        .checked_sub(1)
                        .unwrap_or(app.directory.projects.len() - 1);
                    app.refresh_directory()?;
                }
            }
            DirectoryTab::Agents => {
                if !app.directory.agents.is_empty() {
                    app.directory.selected_agent = app
                        .directory
                        .selected_agent
                        .checked_sub(1)
                        .unwrap_or(app.directory.agents.len() - 1);
                }
            }
        },
        KeyCode::Char('n') => {
            if matches!(app.directory.tab, DirectoryTab::Projects) {
                app.input_mode = Some(InputMode::ProjectCreate);
                app.input_buffer.clear();
            }
        }
        KeyCode::Char('a') => {
            if matches!(app.directory.tab, DirectoryTab::Agents) {
                app.input_mode = Some(InputMode::AgentRegister);
                app.input_buffer.clear();
            }
        }
        KeyCode::Char('i') => {
            app.view = View::Inbox;
            app.new_message_flag = false;
        }
        KeyCode::Char('t') => {
            app.view = View::Reservations;
        }
        _ => {}
    }
    Ok(())
}

fn send_compose(app: &mut App) -> Result<()> {
    let Some(identity) = &app.identity else {
        return Err(anyhow!("no agent identity"));
    };
    let to = parse_recipients(&app.compose.to);
    if to.is_empty() {
        app.status_message = "Missing To recipients".to_string();
        return Ok(());
    }
    let message = app.storage.send_message(
        identity.project_id,
        &mailz_core::SendMessage {
            sender_name: identity.agent_name.clone(),
            to,
            cc: Some(parse_recipients(&app.compose.cc)).filter(|v| !v.is_empty()),
            bcc: Some(parse_recipients(&app.compose.bcc)).filter(|v| !v.is_empty()),
            subject: app.compose.subject.clone(),
            body: app.compose.body.clone(),
            importance: None,
            ack_required: None,
            thread_id: None,
        },
    )?;

    app.clear_compose()?;
    app.view = View::Inbox;
    app.inbox.search_query = None;
    app.refresh_inbox()?;
    app.status_message = format!("Sent message {}", message.id);
    Ok(())
}

fn complete_agent_name(app: &mut App) -> bool {
    let field = match app.compose.active {
        ComposeField::To => &mut app.compose.to,
        ComposeField::Cc => &mut app.compose.cc,
        ComposeField::Bcc => &mut app.compose.bcc,
        _ => return false,
    };

    let parts: Vec<&str> = field.split(',').collect();
    let last = parts.last().unwrap_or(&"").trim();
    if last.is_empty() {
        return false;
    }

    let mut matches: Vec<&String> = app
        .agent_names
        .iter()
        .filter(|name| name.to_lowercase().starts_with(&last.to_lowercase()))
        .collect();
    matches.sort();
    let Some(match_name) = matches.first() else {
        return false;
    };

    let prefix = parts[..parts.len().saturating_sub(1)].join(",");
    if prefix.is_empty() {
        *field = match_name.to_string();
    } else {
        *field = format!("{}, {}", prefix, match_name);
    }
    true
}

fn next_compose_field(field: ComposeField) -> ComposeField {
    match field {
        ComposeField::To => ComposeField::Cc,
        ComposeField::Cc => ComposeField::Bcc,
        ComposeField::Bcc => ComposeField::Subject,
        ComposeField::Subject => ComposeField::Body,
        ComposeField::Body => ComposeField::To,
    }
}

fn previous_compose_field(field: ComposeField) -> ComposeField {
    match field {
        ComposeField::To => ComposeField::Body,
        ComposeField::Cc => ComposeField::To,
        ComposeField::Bcc => ComposeField::Cc,
        ComposeField::Subject => ComposeField::Bcc,
        ComposeField::Body => ComposeField::Subject,
    }
}

fn parse_recipients(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|item| item.trim())
        .filter(|item| !item.is_empty())
        .map(|item| item.to_string())
        .collect()
}

fn join_recipients(items: &[String]) -> String {
    items.join(", ")
}

fn ui(f: &mut Frame, app: &App) {
    match app.view {
        View::Inbox => draw_inbox(f, app),
        View::Compose => draw_compose(f, app),
        View::Reservations => draw_reservations(f, app),
        View::Directory => draw_directory(f, app),
        View::MessageDetail => draw_message_detail(f, app),
    }

    if app.input_mode.is_some() {
        draw_input_overlay(f, app);
    }
}

fn draw_inbox(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(f.area());

    let main_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(20),
            Constraint::Percentage(40),
            Constraint::Percentage(40),
        ])
        .split(chunks[0]);

    draw_folder_pane(f, app, main_chunks[0]);
    draw_message_list(f, app, main_chunks[1]);
    draw_message_preview(f, app, main_chunks[2]);
    draw_status_bar(f, app, chunks[1]);
}

fn draw_compose(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(f.area());

    draw_compose_headers(f, app, chunks[0]);
    draw_compose_body(f, app, chunks[1]);
    draw_status_bar(f, app, chunks[2]);
}

fn draw_reservations(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(f.area());

    draw_reservation_list(f, app, chunks[0]);
    draw_status_bar(f, app, chunks[1]);
}

fn draw_directory(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(f.area());

    let main_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(chunks[0]);

    draw_projects_list(f, app, main_chunks[0]);
    draw_agents_list(f, app, main_chunks[1]);
    draw_status_bar(f, app, chunks[1]);
}

fn draw_message_detail(f: &mut Frame, app: &App) {
    let area = centered_rect(80, 80, f.area());
    let block = Block::default().title(" Message ").borders(Borders::ALL);

    let text = if let Some(message) = &app.focused_message {
        Text::from(vec![
            Line::from(format!("From: {}", message.sender)),
            Line::from(format!("Subject: {}", message.subject)),
            Line::from(""),
            Line::from(message.body.clone()),
        ])
    } else {
        Text::from("No message")
    };

    let paragraph = Paragraph::new(text).block(block).wrap(Wrap { trim: false });
    f.render_widget(paragraph, area);
}

fn draw_input_overlay(f: &mut Frame, app: &App) {
    let area = centered_rect(60, 20, f.area());
    let title = match app.input_mode {
        Some(InputMode::Search) => " Search ",
        Some(InputMode::Reserve) => " Reserve files ",
        Some(InputMode::ProjectFilter) => " Filter project ",
        Some(InputMode::ProjectCreate) => " Create project ",
        Some(InputMode::AgentRegister) => " Register agent ",
        None => " Input ",
    };
    let block = Block::default().title(title).borders(Borders::ALL);
    let paragraph = Paragraph::new(app.input_buffer.as_str()).block(block);
    f.render_widget(paragraph, area);
}

fn draw_folder_pane(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default().title(" Folders ").borders(Borders::ALL);
    let unread = app.unread_count;
    let items: Vec<ListItem> = app
        .inbox
        .folders
        .iter()
        .enumerate()
        .map(|(idx, name)| {
            let style = if idx == app.inbox.selected_folder {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let label = if *name == "Inbox" && unread > 0 {
                format!("{name} ({unread})")
            } else {
                name.to_string()
            };
            ListItem::new(label).style(style)
        })
        .collect();
    let list = List::new(items).block(block);
    f.render_widget(list, area);
}

fn draw_message_list(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default().title(" Inbox ").borders(Borders::ALL);
    let items: Vec<ListItem> = app
        .inbox
        .messages
        .iter()
        .enumerate()
        .map(|(idx, message)| {
            let unread = message.read_at.is_none();
            let flag = if unread { "*" } else { " " };
            let line = format!("{} {} - {}", flag, message.sender, message.subject);
            let style = if idx == app.inbox.selected_message {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            ListItem::new(line).style(style)
        })
        .collect();
    let list = List::new(items).block(block);
    f.render_widget(list, area);
}

fn draw_message_preview(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default().title(" Preview ").borders(Borders::ALL);
    let content = if let Some(message) = app.selected_message() {
        let mut lines = Vec::new();
        lines.push(Line::from(format!("From: {}", message.sender)));
        lines.push(Line::from(format!("Subject: {}", message.subject)));
        lines.push(Line::from(""));
        lines.push(Line::from(message.body.clone()));
        Text::from(lines)
    } else {
        Text::from("No message selected")
    };
    let paragraph = Paragraph::new(content)
        .block(block)
        .wrap(Wrap { trim: true });
    f.render_widget(paragraph, area);
}

fn draw_compose_headers(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default().title(" Compose ").borders(Borders::ALL);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let lines = vec![
        compose_line(
            "To",
            &app.compose.to,
            app.compose.active == ComposeField::To,
        ),
        compose_line(
            "Cc",
            &app.compose.cc,
            app.compose.active == ComposeField::Cc,
        ),
        compose_line(
            "Bcc",
            &app.compose.bcc,
            app.compose.active == ComposeField::Bcc,
        ),
        compose_line(
            "Subject",
            &app.compose.subject,
            app.compose.active == ComposeField::Subject,
        ),
    ];
    let paragraph = Paragraph::new(lines);
    f.render_widget(paragraph, inner);
}

fn draw_compose_body(f: &mut Frame, app: &App, area: Rect) {
    let title = if app.compose.active == ComposeField::Body {
        " Body (Ctrl+S to send) "
    } else {
        " Body "
    };
    let block = Block::default().title(title).borders(Borders::ALL);
    let paragraph = Paragraph::new(app.compose.body.as_str())
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(paragraph, area);
}

fn compose_line(label: &str, value: &str, active: bool) -> Line<'static> {
    let style = if active {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    Line::from(vec![
        Span::styled(format!("{label}: "), style),
        Span::raw(value.to_string()),
    ])
}

fn draw_reservation_list(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(" Reservations ")
        .borders(Borders::ALL);
    let now = Utc::now();
    let mut conflict_paths = std::collections::HashMap::new();
    for reservation in &app.reservations.reservations {
        *conflict_paths.entry(&reservation.path_pattern).or_insert(0) += 1;
    }

    let items: Vec<ListItem> = app
        .reservations
        .reservations
        .iter()
        .enumerate()
        .map(|(idx, reservation)| {
            let remaining = reservation.expires_at - now;
            let minutes = remaining.num_minutes();
            let conflict = conflict_paths
                .get(&reservation.path_pattern)
                .copied()
                .unwrap_or(0)
                > 1;
            let expiring = minutes >= 0 && minutes <= 5;
            let mut style = if idx == app.reservations.selected {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            if conflict {
                style = style.fg(Color::Red);
            } else if expiring {
                style = style.fg(Color::LightYellow);
            }
            let line = format!(
                "{} | agent {} | {}m | {}",
                reservation.path_pattern,
                reservation.agent_id,
                minutes,
                if reservation.exclusive {
                    "exclusive"
                } else {
                    "shared"
                }
            );
            ListItem::new(line).style(style)
        })
        .collect();
    let list = List::new(items).block(block);
    f.render_widget(list, area);
}

fn draw_projects_list(f: &mut Frame, app: &App, area: Rect) {
    let active = matches!(app.directory.tab, DirectoryTab::Projects);
    let title = if active {
        " Projects (Tab) "
    } else {
        " Projects "
    };
    let block = Block::default().title(title).borders(Borders::ALL);
    let items: Vec<ListItem> = app
        .directory
        .projects
        .iter()
        .enumerate()
        .map(|(idx, project)| {
            let count = app
                .directory
                .agent_counts
                .get(&project.id)
                .copied()
                .unwrap_or(0);
            let line = format!("{} ({})", project.slug, count);
            let mut style = Style::default();
            if idx == app.directory.selected_project && active {
                style = style.fg(Color::Yellow).add_modifier(Modifier::BOLD);
            }
            ListItem::new(line).style(style)
        })
        .collect();
    let list = List::new(items).block(block);
    f.render_widget(list, area);
}

fn draw_agents_list(f: &mut Frame, app: &App, area: Rect) {
    let active = matches!(app.directory.tab, DirectoryTab::Agents);
    let title = if active { " Agents (Tab) " } else { " Agents " };
    let block = Block::default().title(title).borders(Borders::ALL);
    let items: Vec<ListItem> = app
        .directory
        .agents
        .iter()
        .enumerate()
        .map(|(idx, agent)| {
            let summary = app.directory.summaries.get(&agent.name);
            let line = if let Some(summary) = summary {
                format!(
                    "{} | unread {} | total {} | last {}",
                    agent.name, summary.unread, summary.total, agent.last_active_at
                )
            } else {
                format!("{} | last {}", agent.name, agent.last_active_at)
            };
            let mut style = Style::default();
            if idx == app.directory.selected_agent && active {
                style = style.fg(Color::Yellow).add_modifier(Modifier::BOLD);
            }
            ListItem::new(line).style(style)
        })
        .collect();
    let list = List::new(items).block(block);
    f.render_widget(list, area);
}

fn draw_status_bar(f: &mut Frame, app: &App, area: Rect) {
    let mode_label = match app.view {
        View::Inbox => "INBOX",
        View::Compose => "COMPOSE",
        View::Reservations => "RESERVATIONS",
        View::Directory => "DIRECTORY",
        View::MessageDetail => "MESSAGE",
    };
    let badge = if app.new_message_flag {
        format!(" NEW ({}) ", app.unread_count)
    } else if app.unread_count > 0 {
        format!(" UNREAD {} ", app.unread_count)
    } else {
        String::new()
    };
    let mut parts = vec![Span::styled(
        format!(" {mode_label} "),
        Style::default().fg(Color::Black).bg(Color::Green),
    )];
    if !badge.is_empty() {
        parts.push(Span::styled(
            badge,
            Style::default().fg(Color::Black).bg(Color::Yellow),
        ));
    }
    parts.push(Span::raw(" "));
    parts.push(Span::raw(app.status_message.clone()));

    let status = Line::from(parts);

    f.render_widget(Paragraph::new(status), area);
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_recipients_splits_and_trims() {
        let recipients = parse_recipients("a, b , , c");
        assert_eq!(recipients, vec!["a", "b", "c"]);
    }

    #[test]
    fn join_recipients_roundtrip() {
        let items = vec!["a".to_string(), "b".to_string()];
        assert_eq!(join_recipients(&items), "a, b");
    }
}
