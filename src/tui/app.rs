use super::components::{ButtonHit, contains};
use crate::config::{Config, Layout, Mode, ProviderConfig};
use crate::proposal::{ProposeProgress, ProviderAllow, ReviewItem, ReviewStatus};
use anyhow::{Result, anyhow};
use chrono::Local;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Instant;

const COMMANDS: &[&str] = &[
    "approve", "clear", "filter", "help", "index", "init", "propose", "quit", "refresh", "reject",
    "review", "tab", "verify",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Review,
    Activity,
    Workspace,
}

impl Tab {
    pub const ALL: [Self; 3] = [Self::Review, Self::Activity, Self::Workspace];

    pub fn title(self) -> &'static str {
        match self {
            Self::Review => "Review",
            Self::Activity => "Activity",
            Self::Workspace => "Workspace",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Review => Self::Activity,
            Self::Activity => Self::Workspace,
            Self::Workspace => Self::Review,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Index,
    Verify,
    Propose {
        path: Option<PathBuf>,
        jobs: Option<usize>,
    },
    Approve(String),
    Reject(String),
    Init,
}

impl Action {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Index => "index",
            Self::Verify => "verify",
            Self::Propose { .. } => "propose",
            Self::Approve(_) => "approve",
            Self::Reject(_) => "reject",
            Self::Init => "init",
        }
    }

    pub fn needs_confirmation(&self) -> bool {
        matches!(
            self,
            Self::Propose { .. } | Self::Approve(_) | Self::Reject(_)
        )
    }
}

#[derive(Debug, Clone)]
pub struct Confirmation {
    pub action: Action,
    pub title: String,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct JobState {
    pub label: String,
    pub detail: String,
    pub progress: Option<(usize, usize)>,
    pub started: Instant,
}

#[derive(Debug)]
enum WorkerEvent {
    Progress {
        detail: String,
        progress: Option<(usize, usize)>,
    },
    Finished {
        label: String,
        result: std::result::Result<String, String>,
    },
}

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub at: String,
    pub level: LogLevel,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Debug, Clone)]
pub struct WorkspaceInfo {
    pub initialized: bool,
    pub mode: String,
    pub provider: String,
    pub tracked_files: usize,
    pub config_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct SourceLine {
    pub number: usize,
    pub text: String,
    pub matched: bool,
}

#[derive(Debug, Clone, Copy)]
pub enum HitAction {
    Tab(Tab),
    CommandFocus,
    Toolbar(ToolbarAction),
    Confirm,
    Cancel,
}

#[derive(Debug, Clone, Copy)]
pub enum ToolbarAction {
    Index,
    Verify,
    Propose,
    Approve,
    Reject,
}

pub struct App {
    pub root: PathBuf,
    pub tab: Tab,
    pub reviews: Vec<ReviewItem>,
    pub filtered: Vec<usize>,
    pub selected: usize,
    pub list_offset: usize,
    pub list_rows: usize,
    pub include_resolved: bool,
    pub filter: String,
    pub command: String,
    pub command_mode: bool,
    pub history: Vec<String>,
    pub history_cursor: Option<usize>,
    pub show_help: bool,
    pub confirmation: Option<Confirmation>,
    pub job: Option<JobState>,
    pub logs: VecDeque<LogEntry>,
    pub workspace: WorkspaceInfo,
    pub should_quit: bool,
    pub tick: usize,
    pub mouse: (u16, u16),
    pub hits: Vec<ButtonHit<HitAction>>,
    pub review_area: Option<Rect>,
    tx: Sender<WorkerEvent>,
    rx: Receiver<WorkerEvent>,
}

impl App {
    pub fn new(root: &Path) -> Self {
        let (tx, rx) = mpsc::channel();
        let mut app = Self {
            root: root.to_path_buf(),
            tab: Tab::Review,
            reviews: Vec::new(),
            filtered: Vec::new(),
            selected: 0,
            list_offset: 0,
            list_rows: 1,
            include_resolved: false,
            filter: String::new(),
            command: String::new(),
            command_mode: false,
            history: Vec::new(),
            history_cursor: None,
            show_help: false,
            confirmation: None,
            job: None,
            logs: VecDeque::new(),
            workspace: workspace_info(root),
            should_quit: false,
            tick: 0,
            mouse: (0, 0),
            hits: Vec::new(),
            review_area: None,
            tx,
            rx,
        };
        app.refresh_reviews();
        app.log(LogLevel::Info, "interactive workspace ready");
        app
    }

    pub fn selected_review(&self) -> Option<&ReviewItem> {
        let index = *self.filtered.get(self.selected)?;
        self.reviews.get(index)
    }

    pub fn source_context(&self) -> Vec<SourceLine> {
        self.selected_review()
            .map(|item| source_context(&self.root, item, 5))
            .unwrap_or_default()
    }

    pub fn command_suggestions(&self) -> Vec<&'static str> {
        let prefix = self.command.split_whitespace().next().unwrap_or("");
        if prefix.is_empty() {
            return COMMANDS.iter().copied().take(6).collect();
        }
        COMMANDS
            .iter()
            .copied()
            .filter(|command| command.starts_with(prefix))
            .take(6)
            .collect()
    }

    pub fn poll_workers(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        while let Ok(event) = self.rx.try_recv() {
            match event {
                WorkerEvent::Progress { detail, progress } => {
                    if let Some(job) = &mut self.job {
                        job.detail = detail;
                        job.progress = progress;
                    }
                }
                WorkerEvent::Finished { label, result } => {
                    self.job = None;
                    match result {
                        Ok(message) => self.log(LogLevel::Success, message),
                        Err(message) => self.log(LogLevel::Error, format!("{label}: {message}")),
                    }
                    self.workspace = workspace_info(&self.root);
                    self.refresh_reviews();
                }
            }
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if self.show_help {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('?') | KeyCode::Enter) {
                self.show_help = false;
            }
            return;
        }
        if self.confirmation.is_some() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Enter => self.accept_confirmation(),
                KeyCode::Char('n') | KeyCode::Esc => self.confirmation = None,
                _ => {}
            }
            return;
        }
        if self.command_mode {
            self.handle_command_key(key);
            return;
        }
        match key.code {
            KeyCode::Char('q') => self.request_quit(),
            KeyCode::Char('?') => self.show_help = true,
            KeyCode::Char(':') => self.focus_command(""),
            KeyCode::Char('/') => self.focus_command("filter "),
            KeyCode::Char('j') | KeyCode::Down => self.select_next(),
            KeyCode::Char('k') | KeyCode::Up => self.select_previous(),
            KeyCode::PageDown => self.select_by(self.list_rows as isize),
            KeyCode::PageUp => self.select_by(-(self.list_rows as isize)),
            KeyCode::Tab => self.tab = self.tab.next(),
            KeyCode::Char('i') => self.request_action(Action::Index),
            KeyCode::Char('v') => self.request_action(Action::Verify),
            KeyCode::Char('p') => self.request_action(Action::Propose {
                path: None,
                jobs: None,
            }),
            KeyCode::Char('a') => self.request_selected(true),
            KeyCode::Char('r') => self.request_selected(false),
            KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.logs.clear()
            }
            _ => {}
        }
    }

    fn handle_command_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.command_mode = false;
                self.command.clear();
                self.history_cursor = None;
            }
            KeyCode::Enter => {
                let command = self.command.trim().to_string();
                self.command_mode = false;
                self.command.clear();
                self.history_cursor = None;
                if !command.is_empty() {
                    self.history.push(command.clone());
                    if let Err(err) = self.execute_command(&command) {
                        self.log(LogLevel::Error, err.to_string());
                    }
                }
            }
            KeyCode::Backspace => {
                self.command.pop();
            }
            KeyCode::Tab => {
                if let Some(completion) = self.command_suggestions().first() {
                    let tail = self
                        .command
                        .split_once(' ')
                        .map(|(_, tail)| tail.to_string());
                    self.command = completion.to_string();
                    if let Some(tail) = tail {
                        self.command.push(' ');
                        self.command.push_str(&tail);
                    }
                }
            }
            KeyCode::Up => self.history_previous(),
            KeyCode::Down => self.history_next(),
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.command.push(character);
            }
            _ => {}
        }
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        self.mouse = (mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(action) = self
                    .hits
                    .iter()
                    .find_map(|hit| hit.action_at(mouse.column, mouse.row))
                {
                    self.handle_hit(action);
                    return;
                }
                if let Some(area) = self.review_area
                    && contains(area, mouse.column, mouse.row)
                {
                    let row = mouse.row.saturating_sub(area.y) as usize;
                    let selected = self.list_offset + row;
                    if selected < self.filtered.len() {
                        self.selected = selected;
                    }
                }
            }
            MouseEventKind::ScrollDown => self.select_next(),
            MouseEventKind::ScrollUp => self.select_previous(),
            _ => {}
        }
    }

    fn handle_hit(&mut self, action: HitAction) {
        match action {
            HitAction::Tab(tab) => self.tab = tab,
            HitAction::CommandFocus => self.focus_command(""),
            HitAction::Toolbar(action) => match action {
                ToolbarAction::Index => self.request_action(Action::Index),
                ToolbarAction::Verify => self.request_action(Action::Verify),
                ToolbarAction::Propose => self.request_action(Action::Propose {
                    path: None,
                    jobs: None,
                }),
                ToolbarAction::Approve => self.request_selected(true),
                ToolbarAction::Reject => self.request_selected(false),
            },
            HitAction::Confirm => self.accept_confirmation(),
            HitAction::Cancel => self.confirmation = None,
        }
    }

    fn execute_command(&mut self, raw: &str) -> Result<()> {
        let mut parts = raw.split_whitespace();
        let command = parts.next().unwrap_or_default();
        match command {
            "quit" | "q" => self.request_quit(),
            "help" | "?" => self.show_help = true,
            "clear" => self.logs.clear(),
            "refresh" => {
                self.refresh_reviews();
                self.workspace = workspace_info(&self.root);
                self.log(LogLevel::Info, "workspace refreshed");
            }
            "init" => self.request_action(Action::Init),
            "index" => self.request_action(Action::Index),
            "verify" => self.request_action(Action::Verify),
            "review" => {
                self.include_resolved = matches!(parts.next(), Some("all"));
                self.tab = Tab::Review;
                self.refresh_reviews();
            }
            "filter" => {
                self.filter = parts.collect::<Vec<_>>().join(" ");
                self.apply_filter();
            }
            "tab" => {
                self.tab = match parts.next() {
                    Some("review") => Tab::Review,
                    Some("activity") => Tab::Activity,
                    Some("workspace") => Tab::Workspace,
                    _ => return Err(anyhow!("usage: tab review|activity|workspace")),
                };
            }
            "approve" => {
                let id = parts
                    .next()
                    .map(str::to_string)
                    .or_else(|| self.selected_review().map(|item| item.id.clone()))
                    .ok_or_else(|| anyhow!("nothing selected"))?;
                self.request_action(Action::Approve(id));
            }
            "reject" => {
                let id = parts
                    .next()
                    .map(str::to_string)
                    .or_else(|| self.selected_review().map(|item| item.id.clone()))
                    .ok_or_else(|| anyhow!("nothing selected"))?;
                self.request_action(Action::Reject(id));
            }
            "propose" => {
                let args = parts.collect::<Vec<_>>();
                let mut path = None;
                let mut jobs = None;
                let mut index = 0;
                while index < args.len() {
                    match args[index] {
                        "-j" | "--jobs" => {
                            let raw_jobs = args
                                .get(index + 1)
                                .ok_or_else(|| anyhow!("missing jobs value"))?;
                            let parsed = raw_jobs.parse::<usize>()?;
                            if !(1..=32).contains(&parsed) {
                                return Err(anyhow!("jobs must be between 1 and 32"));
                            }
                            jobs = Some(parsed);
                            index += 2;
                        }
                        value if path.is_none() => {
                            path = Some(PathBuf::from(value));
                            index += 1;
                        }
                        value => return Err(anyhow!("unexpected propose argument: {value}")),
                    }
                }
                self.request_action(Action::Propose { path, jobs });
            }
            "" => {}
            _ => return Err(anyhow!("unknown command `{command}`; type `help`")),
        }
        Ok(())
    }

    fn focus_command(&mut self, initial: &str) {
        self.command_mode = true;
        self.command = initial.to_string();
        self.history_cursor = None;
    }

    fn request_quit(&mut self) {
        if self.job.is_some() {
            self.log(
                LogLevel::Warning,
                "an operation is still running; wait for it to finish before quitting",
            );
        } else {
            self.should_quit = true;
        }
    }

    fn request_selected(&mut self, approve: bool) {
        let Some(id) = self.selected_review().map(|item| item.id.clone()) else {
            self.log(LogLevel::Warning, "review queue is empty");
            return;
        };
        self.request_action(if approve {
            Action::Approve(id)
        } else {
            Action::Reject(id)
        });
    }

    fn request_action(&mut self, action: Action) {
        if self.job.is_some() {
            self.log(LogLevel::Warning, "wait for the active operation to finish");
            return;
        }
        if action.needs_confirmation() {
            let (title, message) = confirmation_copy(&self.root, &action);
            self.confirmation = Some(Confirmation {
                action,
                title,
                message,
            });
        } else {
            self.start_action(action);
        }
    }

    fn accept_confirmation(&mut self) {
        if let Some(confirmation) = self.confirmation.take() {
            self.start_action(confirmation.action);
        }
    }

    fn start_action(&mut self, action: Action) {
        let label = action.label().to_string();
        self.job = Some(JobState {
            label: label.clone(),
            detail: "starting".to_string(),
            progress: None,
            started: Instant::now(),
        });
        self.tab = Tab::Activity;
        self.log(LogLevel::Info, format!("started {label}"));
        let root = self.root.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = run_action(&root, action, &tx).map_err(|error| format!("{error:#}"));
            let _ = tx.send(WorkerEvent::Finished { label, result });
        });
    }

    fn refresh_reviews(&mut self) {
        match crate::proposal::list_review(&self.root, self.include_resolved) {
            Ok(items) => self.reviews = items,
            Err(error) => {
                self.reviews.clear();
                if self.workspace.initialized {
                    self.log(LogLevel::Warning, format!("review queue: {error:#}"));
                }
            }
        }
        self.apply_filter();
    }

    fn apply_filter(&mut self) {
        let needle = self.filter.to_lowercase();
        self.filtered = self
            .reviews
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                needle.is_empty()
                    || item.file.to_lowercase().contains(&needle)
                    || item.proposal.original_text.to_lowercase().contains(&needle)
                    || item
                        .proposal
                        .sanitized_text
                        .to_lowercase()
                        .contains(&needle)
                    || item.proposal.category.to_lowercase().contains(&needle)
                    || item.flag.to_lowercase().contains(&needle)
            })
            .map(|(index, _)| index)
            .collect();
        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
        self.ensure_selection_visible();
    }

    fn select_next(&mut self) {
        if !self.filtered.is_empty() {
            self.selected = (self.selected + 1).min(self.filtered.len() - 1);
            self.ensure_selection_visible();
        }
    }

    fn select_previous(&mut self) {
        self.selected = self.selected.saturating_sub(1);
        self.ensure_selection_visible();
    }

    fn select_by(&mut self, delta: isize) {
        if self.filtered.is_empty() {
            return;
        }
        self.selected = self
            .selected
            .saturating_add_signed(delta)
            .min(self.filtered.len() - 1);
        self.ensure_selection_visible();
    }

    fn ensure_selection_visible(&mut self) {
        if self.selected < self.list_offset {
            self.list_offset = self.selected;
        } else if self.selected >= self.list_offset + self.list_rows {
            self.list_offset = self.selected + 1 - self.list_rows;
        }
    }

    fn history_previous(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = self
            .history_cursor
            .unwrap_or(self.history.len())
            .saturating_sub(1);
        self.history_cursor = Some(next);
        self.command = self.history[next].clone();
    }

    fn history_next(&mut self) {
        let Some(cursor) = self.history_cursor else {
            return;
        };
        if cursor + 1 < self.history.len() {
            self.history_cursor = Some(cursor + 1);
            self.command = self.history[cursor + 1].clone();
        } else {
            self.history_cursor = None;
            self.command.clear();
        }
    }

    pub fn log(&mut self, level: LogLevel, message: impl Into<String>) {
        self.logs.push_back(LogEntry {
            at: Local::now().format("%H:%M:%S").to_string(),
            level,
            message: message.into(),
        });
        while self.logs.len() > 500 {
            self.logs.pop_front();
        }
    }
}

fn run_action(root: &Path, action: Action, tx: &Sender<WorkerEvent>) -> Result<String> {
    match action {
        Action::Init => {
            let layout = crate::index::init_workspace(root)?;
            Ok(format!("initialized {}", layout.state_dir.display()))
        }
        Action::Index => {
            let report = crate::index::index_workspace(root)?;
            Ok(format!(
                "index complete: {} indexed, {} unchanged, {} skipped, {} errors",
                report.indexed,
                report.unchanged,
                report.skipped,
                report.errors.len()
            ))
        }
        Action::Verify => {
            let report = crate::verify::verify_workspace(root)?;
            if report.is_ok() {
                Ok(format!("verified {} tracked files", report.checked))
            } else {
                Err(anyhow!("verify found {} issue(s)", report.failures.len()))
            }
        }
        Action::Approve(id) => {
            let item = crate::proposal::resolve_review(root, &id, true)?;
            Ok(format!(
                "approved {} -> {} in {}",
                item.proposal.original_text, item.proposal.sanitized_text, item.file
            ))
        }
        Action::Reject(id) => {
            let item = crate::proposal::resolve_review(root, &id, false)?;
            Ok(format!("rejected {}", item.proposal.original_text))
        }
        Action::Propose { path, jobs } => {
            let progress_tx = tx.clone();
            let report = crate::proposal::propose_sanitize_with_progress(
                root,
                path.as_deref(),
                ProviderAllow {
                    command: true,
                    endpoint: true,
                },
                jobs,
                move |progress| {
                    let (detail, fraction) = progress_description(progress);
                    let _ = progress_tx.send(WorkerEvent::Progress {
                        detail,
                        progress: fraction,
                    });
                },
            )?;
            Ok(format!(
                "proposal scan complete: {} queued, {} duplicates, {} rejected, {} errors",
                report.queued,
                report.duplicates,
                report.rejected.len(),
                report.errors.len()
            ))
        }
    }
}

fn progress_description(progress: ProposeProgress) -> (String, Option<(usize, usize)>) {
    match progress {
        ProposeProgress::Started {
            total,
            jobs,
            requests,
        } => (
            format!("{total} files, {requests} requests, {jobs} workers"),
            Some((0, requests)),
        ),
        ProposeProgress::FileStarted { file, chunks, .. } => {
            (format!("scanning {file} ({chunks} chunks)"), None)
        }
        ProposeProgress::ChunkStarted {
            file,
            chunk,
            chunks,
        } => (format!("{file} chunk {chunk}/{chunks}"), None),
        ProposeProgress::ChunkFinished {
            completed,
            total,
            file,
            ..
        } => (format!("completed {file}"), Some((completed, total))),
        ProposeProgress::FileFinished {
            completed,
            total,
            file,
            queued,
            ..
        } => (format!("{file}: {queued} queued"), Some((completed, total))),
        ProposeProgress::Finished { total, queued, .. } => (
            format!("finished {total} files, {queued} queued"),
            Some((total, total)),
        ),
    }
}

fn workspace_info(root: &Path) -> WorkspaceInfo {
    let layout = Layout::new(root);
    let initialized = layout.state_dir.is_dir();
    let config = initialized
        .then(|| Config::load_or_default_lenient(&layout).ok())
        .flatten();
    let mode = config
        .as_ref()
        .map(|config| match config.mode {
            Mode::Soft => "soft",
            Mode::Guided => "guided",
            Mode::Strict => "strict",
        })
        .unwrap_or("not initialized")
        .to_string();
    let provider = config
        .as_ref()
        .map(|config| provider_name(&config.sanitizer.provider))
        .unwrap_or_else(|| "none".to_string());
    let tracked_files = crate::db::connect(&layout)
        .and_then(|connection| crate::db::tracked_files(&connection))
        .map(|files| files.len())
        .unwrap_or(0);
    WorkspaceInfo {
        initialized,
        mode,
        provider,
        tracked_files,
        config_path: layout.config_path,
    }
}

fn provider_name(provider: &ProviderConfig) -> String {
    match provider {
        ProviderConfig::Stub => "heuristic".to_string(),
        ProviderConfig::External { command, .. } => command
            .first()
            .cloned()
            .unwrap_or_else(|| "external".to_string()),
        ProviderConfig::LlmStub { .. } => "llm-stub".to_string(),
        ProviderConfig::Llm { model, .. }
        | ProviderConfig::Openrouter { model, .. }
        | ProviderConfig::KouRouter { model, .. } => model.clone(),
    }
}

fn confirmation_copy(root: &Path, action: &Action) -> (String, String) {
    match action {
        Action::Propose { path, .. } => {
            let layout = Layout::new(root);
            let destination = Config::load_or_default_lenient(&layout)
                .ok()
                .and_then(|config| config.sanitizer.provider.llm_endpoint())
                .map(|endpoint| format!("{} using {}", endpoint.base_url, endpoint.model))
                .unwrap_or_else(|| "the configured provider".to_string());
            let scope = path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "the full indexed workspace".to_string());
            (
                "Run proposal scan?".to_string(),
                format!(
                    "Real source from {scope} may be sent to {destination}. The model can only queue proposals; it cannot modify source files."
                ),
            )
        }
        Action::Approve(id) => (
            "Approve replacement?".to_string(),
            format!(
                "Proposal {id} will enter the global alias registry and trigger deterministic reindexing."
            ),
        ),
        Action::Reject(id) => (
            "Reject proposal?".to_string(),
            format!("Proposal {id} will be removed from the pending queue."),
        ),
        _ => ("Confirm action".to_string(), action.label().to_string()),
    }
}

pub fn source_context(root: &Path, item: &ReviewItem, radius: usize) -> Vec<SourceLine> {
    let Ok(content) = std::fs::read_to_string(root.join(&item.file)) else {
        return Vec::new();
    };
    let lines = content.lines().collect::<Vec<_>>();
    let first = lines
        .iter()
        .position(|line| line.contains(&item.proposal.original_text))
        .unwrap_or(0);
    let start = first.saturating_sub(radius);
    let end = (first + radius + 1).min(lines.len());
    lines[start..end]
        .iter()
        .enumerate()
        .map(|(offset, line)| SourceLine {
            number: start + offset + 1,
            text: (*line).to_string(),
            matched: line.contains(&item.proposal.original_text),
        })
        .collect()
}

pub fn is_pending(item: &ReviewItem) -> bool {
    item.status == ReviewStatus::Pending
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proposal::Proposal;

    #[test]
    fn source_context_centers_the_matching_line() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("main.rs"),
            "one\ntwo\nlet hwid = 3;\nfour\nfive\n",
        )
        .unwrap();
        let item = ReviewItem {
            id: "id".to_string(),
            file: "main.rs".to_string(),
            proposal: Proposal {
                category: "identifier".to_string(),
                original_text: "hwid".to_string(),
                sanitized_text: "device_id".to_string(),
                confidence: 0.9,
                rationale: None,
            },
            status: ReviewStatus::Pending,
            flag: "clean".to_string(),
            created_at: String::new(),
        };
        let context = source_context(temp.path(), &item, 1);
        assert_eq!(
            context.iter().map(|line| line.number).collect::<Vec<_>>(),
            vec![2, 3, 4]
        );
        assert!(context[1].matched);
    }

    #[test]
    fn button_hit_uses_half_open_bounds() {
        let hit = ButtonHit {
            area: Rect::new(2, 3, 4, 2),
            action: HitAction::Confirm,
        };
        assert!(hit.action_at(2, 3).is_some());
        assert!(hit.action_at(5, 4).is_some());
        assert!(hit.action_at(6, 4).is_none());
    }

    #[test]
    fn quit_waits_for_an_active_worker() {
        let temp = tempfile::tempdir().unwrap();
        let mut app = App::new(temp.path());
        app.job = Some(JobState {
            label: "verify".to_string(),
            detail: "running".to_string(),
            progress: None,
            started: Instant::now(),
        });
        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(!app.should_quit);
        assert!(app.logs.back().unwrap().message.contains("still running"));
    }
}
