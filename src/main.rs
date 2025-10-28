use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use skim::prelude::*;
use std::fs;
use std::io::Cursor;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

const STATE_VERSION: u32 = 1;
const MAX_HISTORY_SIZE: usize = 10;
const CACHE_TTL_SECONDS: i64 = 3600;
const PROJECT_SCAN_MIN_DEPTH: usize = 2;
const PROJECT_SCAN_MAX_DEPTH: usize = 2;
const FILES_WINDOW_INDEX: u32 = 9;
const EDITOR_WINDOW_INDEX: u32 = 1;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Parser)]
#[command(name = "ws")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Pick a project or session
    Pick {
        #[arg(long, default_value = "~/workspace")]
        workspace: String,
    },
    /// Kill a session (switches to previous)
    Kill,
    /// Jump back to previous session
    Back,
    /// Refresh project cache
    Refresh {
        #[arg(long, default_value = "~/workspace")]
        workspace: String,
    },
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct ProjectInfo {
    path: String,
    category: String,
    name: String,
}

impl ProjectInfo {
    fn display_name(&self) -> String {
        format!("{}/{}", self.category, self.name)
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct SessionInfo {
    name: String,
    last_active: i64,
}

#[derive(Debug, Clone)]
enum SelectableItem {
    Session(String),
    Project(ProjectInfo),
}

impl SelectableItem {
    fn to_display_string(&self) -> String {
        match self {
            Self::Session(name) => format!("session: {}", name),
            Self::Project(info) => format!("project: {}", info.display_name()),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct State {
    version: u32,
    history: Vec<String>,
    cache: ProjectCache,
}

#[derive(Debug, Serialize, Deserialize)]
struct ProjectCache {
    projects: Vec<ProjectInfo>,
    updated_at: i64,
    ttl: i64,
}

impl State {
    fn load() -> Self {
        let state_path = Self::state_path();
        fs::read_to_string(&state_path)
            .ok()
            .and_then(|contents| serde_json::from_str(&contents).ok())
            .unwrap_or_default()
    }

    fn save(&self) -> Result<()> {
        let state_path = Self::state_path();
        if let Some(parent) = state_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        fs::write(&state_path, json)?;
        Ok(())
    }

    fn state_path() -> PathBuf {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("ws")
            .join("state.json")
    }

    fn push_history(&mut self, session: String) {
        self.history.retain(|s| s != &session);
        self.history.push(session);
        if self.history.len() > MAX_HISTORY_SIZE {
            self.history.remove(0);
        }
    }

    fn previous_session(&self) -> Option<&str> {
        if self.history.len() >= 2 {
            Some(&self.history[self.history.len() - 2])
        } else {
            self.history.last().map(|s| s.as_str())
        }
    }

    fn cache_valid(&self) -> bool {
        let now = current_timestamp();
        now - self.cache.updated_at < self.cache.ttl
    }

    fn refresh_cache(&mut self, workspace: &str) -> Result<()> {
        self.cache.projects = scan_projects(workspace)?;
        self.cache.updated_at = current_timestamp();
        Ok(())
    }

    fn ensure_cache_valid(&mut self, workspace: &str) -> Result<()> {
        if !self.cache_valid() {
            self.refresh_cache(workspace)?;
        }
        Ok(())
    }
}

impl Default for State {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            history: Vec::new(),
            cache: ProjectCache {
                projects: Vec::new(),
                updated_at: 0,
                ttl: CACHE_TTL_SECONDS,
            },
        }
    }
}

struct TmuxClient;

impl TmuxClient {
    fn is_in_tmux() -> bool {
        std::env::var("TMUX").is_ok()
    }

    fn current_session() -> Result<String> {
        let output = Command::new("tmux")
            .args(["display-message", "-p", "#{session_name}"])
            .output()?;

        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
        } else {
            Err("Failed to get current session".into())
        }
    }

    fn list_sessions() -> Result<Vec<SessionInfo>> {
        let output = Command::new("tmux")
            .args([
                "list-sessions",
                "-F",
                "#{session_name}|#{session_last_attached}",
            ])
            .output()?;

        if !output.status.success() {
            return Ok(Vec::new());
        }

        let sessions = String::from_utf8_lossy(&output.stdout);
        Ok(sessions
            .lines()
            .filter_map(|line| {
                let parts: Vec<&str> = line.split('|').collect();
                if parts.len() >= 2 {
                    Some(SessionInfo {
                        name: parts[0].to_string(),
                        last_active: parts[1].parse().unwrap_or(0),
                    })
                } else {
                    None
                }
            })
            .collect())
    }

    fn has_session(name: &str) -> Result<bool> {
        let status = Command::new("tmux")
            .args(["has-session", "-t", name])
            .status()?;
        Ok(status.success())
    }

    fn create_session(name: &str, path: &str) -> Result<()> {
        Command::new("tmux")
            .args([
                "new-session",
                "-d",
                "-s",
                name,
                "-c",
                path,
                "-n",
                "editor",
                "fish -C \"hx\"",
            ])
            .status()?;

        Command::new("tmux")
            .args([
                "new-window",
                "-t",
                &format!("{}:{}", name, FILES_WINDOW_INDEX),
                "-c",
                path,
                "-n",
                "files",
                "fx",
            ])
            .status()?;

        Command::new("tmux")
            .args([
                "select-window",
                "-t",
                &format!("{}:{}", name, EDITOR_WINDOW_INDEX),
            ])
            .status()?;

        Ok(())
    }

    fn switch_client(name: &str) -> Result<()> {
        Command::new("tmux")
            .args(["switch-client", "-t", name])
            .status()?;
        Ok(())
    }

    fn attach_session(name: &str) -> Result<()> {
        Command::new("tmux")
            .args(["attach-session", "-t", name])
            .status()?;
        Ok(())
    }

    fn kill_session(name: &str) -> Result<()> {
        Command::new("tmux")
            .args(["kill-session", "-t", name])
            .status()?;
        Ok(())
    }

    fn switch_or_attach(name: &str) -> Result<()> {
        if Self::is_in_tmux() {
            Self::switch_client(name)
        } else {
            Self::attach_session(name)
        }
    }
}

fn scan_projects(workspace: &str) -> Result<Vec<ProjectInfo>> {
    let workspace = shellexpand::tilde(workspace).to_string();
    let mut projects = Vec::new();

    for entry in WalkDir::new(&workspace)
        .min_depth(PROJECT_SCAN_MIN_DEPTH)
        .max_depth(PROJECT_SCAN_MAX_DEPTH)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_dir())
    {
        if let Some(parent) = entry.path().parent() {
            if let (Some(category), Some(name)) = (
                parent.file_name().and_then(|n| n.to_str()),
                entry.file_name().to_str(),
            ) {
                projects.push(ProjectInfo {
                    path: entry.path().to_string_lossy().to_string(),
                    category: category.to_string(),
                    name: name.to_string(),
                });
            }
        }
    }

    projects.sort_by(|a, b| {
        a.category
            .cmp(&b.category)
            .then_with(|| a.name.cmp(&b.name))
    });

    Ok(projects)
}

struct Picker;

impl Picker {
    fn pick(items: &[String], prompt: &str) -> Option<usize> {
        let options = SkimOptionsBuilder::default()
            .height(Some("100%"))
            .multi(false)
            .color(Some("bw"))
            .layout("reverse")
            .prompt(Some(prompt))
            .build()
            .unwrap();

        let item_reader = SkimItemReader::default();
        let items_str = items.join("\n");
        let skim_items = item_reader.of_bufread(Cursor::new(items_str));

        let output = Skim::run_with(&options, Some(skim_items))?;

        if output.is_abort {
            None
        } else {
            output.selected_items.first().and_then(|item| {
                let selected_text = item.output().to_string();
                items
                    .iter()
                    .position(|s| s == &selected_text)
            })
        }
    }
}

fn handle_pick_command(workspace: &str) -> Result<()> {
    let mut state = State::load();
    state.ensure_cache_valid(workspace)?;

    let in_tmux = TmuxClient::is_in_tmux();
    let sessions = if in_tmux {
        TmuxClient::list_sessions().unwrap_or_default()
    } else {
        Vec::new()
    };

    let mut selectable_items = Vec::new();

    for session in &sessions {
        selectable_items.push(SelectableItem::Session(session.name.clone()));
    }

    for project in &state.cache.projects {
        selectable_items.push(SelectableItem::Project(project.clone()));
    }

    let mut display_strings: Vec<String> = selectable_items
        .iter()
        .map(|item| item.to_display_string())
        .collect();

    let separator_offset = if in_tmux && !sessions.is_empty() && !state.cache.projects.is_empty() {
        display_strings.insert(sessions.len(), "---".to_string());
        1
    } else {
        0
    };

    let selected_index = match Picker::pick(&display_strings, "> ") {
        Some(idx) => idx,
        None => return Ok(()),
    };

    let adjusted_index = if separator_offset > 0 && selected_index >= sessions.len() {
        selected_index - separator_offset
    } else {
        selected_index
    };

    if separator_offset > 0 && selected_index == sessions.len() {
        return Ok(());
    }

    let item = selectable_items
        .get(adjusted_index)
        .ok_or("Invalid selection")?;

    handle_selection(item.clone(), &mut state)?;
    state.save()?;

    Ok(())
}

fn handle_selection(item: SelectableItem, state: &mut State) -> Result<()> {
    match item {
        SelectableItem::Session(name) => {
            state.push_history(name.clone());
            TmuxClient::switch_or_attach(&name)?;
        }
        SelectableItem::Project(project) => {
            let session_name = &project.name;

            if !TmuxClient::has_session(session_name)? {
                TmuxClient::create_session(session_name, &project.path)?;
            }

            state.push_history(session_name.clone());
            TmuxClient::switch_or_attach(session_name)?;
        }
    }

    Ok(())
}

fn handle_kill_command() -> Result<()> {
    let sessions = TmuxClient::list_sessions()?;
    if sessions.is_empty() {
        eprintln!("No sessions to kill");
        return Ok(());
    }

    let mut state = State::load();
    let current = TmuxClient::current_session().ok();

    let session_names: Vec<String> = sessions.iter().map(|s| s.name.clone()).collect();

    let selected_index = match Picker::pick(&session_names, "kill> ") {
        Some(idx) => idx,
        None => return Ok(()),
    };

    let selected = &session_names[selected_index];
    let previous = state.previous_session().map(|s| s.to_string());

    TmuxClient::kill_session(selected)?;

    if current.as_deref() == Some(selected.as_str()) {
        if let Some(prev) = previous {
            if prev != *selected {
                TmuxClient::switch_client(&prev).ok();
            }
        }
    }

    state.history.retain(|s| s != selected);
    state.save()?;

    Ok(())
}

fn handle_back_command() -> Result<()> {
    let mut state = State::load();

    if let Some(previous) = state.previous_session() {
        let previous = previous.to_string();
        TmuxClient::switch_client(&previous)?;
        
        state.push_history(previous);
        state.save()?;
    } else {
        eprintln!("No previous session in history");
    }

    Ok(())
}

fn handle_refresh_command(workspace: &str) -> Result<()> {
    let mut state = State::load();
    state.refresh_cache(workspace)?;
    state.save()?;
    println!("Cache refreshed: {} projects found", state.cache.projects.len());
    Ok(())
}

fn current_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Pick { workspace } => {
            let workspace = shellexpand::tilde(&workspace).to_string();
            handle_pick_command(&workspace)
        }
        Commands::Kill => handle_kill_command(),
        Commands::Back => handle_back_command(),
        Commands::Refresh { workspace } => {
            let workspace = shellexpand::tilde(&workspace).to_string();
            handle_refresh_command(&workspace)
        }
    };

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}
