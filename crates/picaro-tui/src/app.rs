//! Top-level TUI application: handles key events, draws the screen, owns
//! the `Downloader` and `Picaro` core.

use std::sync::Arc;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use tracing::info;

use picaro_core::Picaro;
use picaro_downloader::{DownloadEvent, Downloader, LogLevel};
use picaro_utils::models::SearchResult;

use crate::tabs::Tab;

pub struct App {
    pub picaro: Arc<Picaro>,
    pub downloader: Arc<Downloader>,
    pub tab: Tab,
    pub input: String,
    pub search_results: Vec<SearchResult>,
    pub search_state: ListState,
    pub log: Vec<(LogLevel, String)>,
    pub log_state: ListState,
    pub download_status: Vec<String>,
    pub download_state: ListState,
    pub quit: bool,
    pub status_message: Option<String>,
    pub input_mode: InputMode,
    pub quality_choice: usize,
    pub settings_focus: SettingsFocus,
    pub module_focus: usize,
    pub module_edit_key: String,
    pub module_edit_value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    None,
    Search,
    Settings,
    LoginEmail,
    LoginPassword,
    ModuleEdit,
    Confirm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsFocus {
    General,
    Modules,
    ModuleDetail(usize),
}

impl App {
    pub fn new(picaro: Arc<Picaro>, downloader: Arc<Downloader>) -> Self {
        Self {
            picaro,
            downloader,
            tab: Tab::Search,
            input: String::new(),
            search_results: Vec::new(),
            search_state: ListState::default(),
            log: Vec::new(),
            log_state: ListState::default(),
            download_status: Vec::new(),
            download_state: ListState::default(),
            quit: false,
            status_message: None,
            input_mode: InputMode::Search,
            quality_choice: 4, // default to hifi
            settings_focus: SettingsFocus::General,
            module_focus: 0,
            module_edit_key: String::new(),
            module_edit_value: String::new(),
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        // Ctrl+C always quits
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        match self.input_mode {
            InputMode::None => self.on_key_normal(key),
            InputMode::Search => self.on_key_search(key),
            InputMode::LoginEmail | InputMode::LoginPassword | InputMode::ModuleEdit => {
                self.on_key_text(key)
            }
            InputMode::Settings => self.on_key_settings(key),
            InputMode::Confirm => self.on_key_confirm(key),
        }
    }

    fn on_key_normal(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Tab => {
                self.tab = self.tab.next();
            }
            KeyCode::BackTab => {
                self.tab = self.tab.prev();
            }
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('s') => {
                self.input_mode = InputMode::Search;
                self.status_message = Some("Type to search, Enter to submit, Esc to cancel".into());
            }
            KeyCode::Char('d') => {
                self.tab = Tab::Downloads;
            }
            KeyCode::Char('l') => {
                self.tab = Tab::Logs;
            }
            KeyCode::Char('c') => {
                self.tab = Tab::Settings;
                self.input_mode = InputMode::Settings;
            }
            _ => {}
        }
    }

    fn on_key_search(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.input_mode = InputMode::None;
                self.status_message = None;
            }
            KeyCode::Enter => {
                let q = self.input.clone();
                let service = self.current_service().to_string();
                self.input.clear();
                self.input_mode = InputMode::None;
                self.status_message = Some(format!("Searching {service} for {q}..."));
                let app = self.clone_for_search();
                let service_clone = service.clone();
                let q_clone = q.clone();
                tokio::spawn(async move {
                    let res = app
                        .downloader
                        .search(
                            &service_clone,
                            &q_clone,
                            picaro_utils::models::DownloadType::track,
                        )
                        .await;
                    if let Err(e) = res {
                        info!("search failed: {e}");
                    }
                });
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(c) => {
                self.input.push(c);
            }
            KeyCode::Down => {
                let i = self.search_state.selected().unwrap_or(0);
                let next = if i + 1 < self.search_results.len() {
                    i + 1
                } else {
                    i
                };
                self.search_state.select(Some(next));
            }
            KeyCode::Up => {
                let i = self.search_state.selected().unwrap_or(0);
                let next = i.saturating_sub(1);
                self.search_state.select(Some(next));
            }
            _ => {}
        }
    }

    fn on_key_text(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.input_mode = InputMode::None;
                self.status_message = None;
            }
            KeyCode::Enter => {
                // no-op in this minimal app
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(c) => {
                self.input.push(c);
            }
            _ => {}
        }
    }

    fn on_key_settings(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.input_mode = InputMode::None;
            }
            KeyCode::Tab => {
                self.settings_focus = match self.settings_focus {
                    SettingsFocus::General => SettingsFocus::Modules,
                    SettingsFocus::Modules => SettingsFocus::General,
                    SettingsFocus::ModuleDetail(_) => SettingsFocus::ModuleDetail(0),
                };
            }
            KeyCode::Up => {
                if let SettingsFocus::ModuleDetail(_) = self.settings_focus {
                    if self.module_focus > 0 {
                        self.module_focus -= 1;
                    }
                }
            }
            KeyCode::Down => {
                if let SettingsFocus::ModuleDetail(_) = self.settings_focus {
                    let module_count = self.picaro.list_modules().len();
                    if self.module_focus + 1 < module_count {
                        self.module_focus += 1;
                    }
                }
            }
            _ => {}
        }
    }

    fn on_key_confirm(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.input_mode = InputMode::None;
                self.status_message = Some("Confirmed".into());
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.input_mode = InputMode::None;
            }
            _ => {}
        }
    }

    pub fn current_service(&self) -> &str {
        // The first non-disabled module in the registry
        for n in self.picaro.list_modules() {
            if let Some(m) = self.picaro.registry().get(&n) {
                if m.information
                    .flags
                    .contains(picaro_utils::models::ModuleFlags::hidden)
                {
                    continue;
                }
                return match m.information.service_name.as_str() {
                    "LRCLIB" | "Musixmatch" => continue,
                    _ => match m.information.service_name.as_str() {
                        s => Box::leak(s.to_string().into_boxed_str()),
                    },
                };
            }
        }
        "qobuz"
    }

    fn clone_for_search(&self) -> Arc<App> {
        // unsafe-free clone: we wrap self back into an Arc-equivalent; the
        // downloader field is already an Arc<Downloader>, so we can wrap self
        // in an Arc here.
        Arc::new(App {
            picaro: self.picaro.clone(),
            downloader: self.downloader.clone(),
            tab: self.tab,
            input: self.input.clone(),
            search_results: self.search_results.clone(),
            search_state: self.search_state.clone(),
            log: self.log.clone(),
            log_state: self.log_state.clone(),
            download_status: self.download_status.clone(),
            download_state: self.download_state.clone(),
            quit: self.quit,
            status_message: self.status_message.clone(),
            input_mode: self.input_mode,
            quality_choice: self.quality_choice,
            settings_focus: self.settings_focus,
            module_focus: self.module_focus,
            module_edit_key: self.module_edit_key.clone(),
            module_edit_value: self.module_edit_value.clone(),
        })
    }
}

pub async fn run(picaro: Arc<Picaro>) -> std::io::Result<()> {
    let download_path = picaro
        .merged_globals
        .get("general")
        .and_then(|v| v.get("download_path"))
        .and_then(|v| v.as_str())
        .unwrap_or("./downloads/")
        .to_string();
    let download_path = if download_path.starts_with("~") {
        if let Some(home) = dirs_home() {
            home + &download_path[1..]
        } else {
            download_path
        }
    } else {
        download_path
    };
    let download_path = std::path::PathBuf::from(download_path);
    std::fs::create_dir_all(&download_path).ok();
    let downloader = Arc::new(Downloader::new(picaro.clone(), download_path));
    let mut app = App::new(picaro.clone(), downloader.clone());

    let mut terminal = ratatui::init();
    let res = run_app(&mut terminal, &mut app).await;
    ratatui::restore();
    res
}

fn drain_events(app: &mut App) {
    while let Ok(event) = app.downloader.receiver().try_recv() {
        match event {
            DownloadEvent::Log { level, message } => {
                app.log.push((level, message));
                if app.log.len() > 1000 {
                    app.log.remove(0);
                }
            }
            DownloadEvent::TrackSucceeded { name, location, .. } => {
                app.download_status
                    .push(format!("[ok] {} -> {}", name, location.display()));
            }
            DownloadEvent::TrackSkipped { name, location, .. } => {
                app.download_status
                    .push(format!("[skip] {} -> {}", name, location.display()));
            }
            DownloadEvent::TrackFailed { name, reason, .. } => {
                app.download_status
                    .push(format!("[fail] {} ({})", name, reason));
            }
            DownloadEvent::SearchResults { results, .. } => {
                app.search_results = results;
                app.search_state.select(Some(0));
                app.status_message = Some("Enter to download, Up/Down to navigate".into());
            }
            DownloadEvent::Started { context, .. } => {
                app.status_message = Some(format!("Started: {context}"));
            }
            DownloadEvent::Finished {
                succeeded,
                skipped,
                failed,
                ..
            } => {
                app.status_message = Some(format!(
                    "Done: {succeeded} ok, {skipped} skipped, {failed} failed"
                ));
            }
            DownloadEvent::Error { message } => {
                app.log.push((LogLevel::Error, message));
            }
            _ => {}
        }
    }
}

fn dirs_home() -> Option<String> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(|s| s.to_string_lossy().to_string())
}

async fn run_app<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
) -> std::io::Result<()> {
    loop {
        drain_events(app);
        terminal.draw(|f| ui(f, app))?;
        if crossterm::event::poll(std::time::Duration::from_millis(100))? {
            let event = crossterm::event::read()?;
            if let Event::Key(key) = event {
                app.on_key(key);
            }
        }
        if app.quit {
            return Ok(());
        }
    }
}

fn ui(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(3),
        ])
        .split(area);

    // Header
    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            "PicaroDL",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("tab: {}", app.tab.name()),
            Style::default().fg(Color::DarkGray),
        ),
    ]))
    .block(Block::default().borders(Borders::ALL).title(" Header "));
    f.render_widget(header, chunks[0]);

    // Body
    match app.tab {
        Tab::Search => render_search(f, app, chunks[1]),
        Tab::Downloads => render_downloads(f, app, chunks[1]),
        Tab::Logs => render_logs(f, app, chunks[1]),
        Tab::Settings => render_settings(f, app, chunks[1]),
    }

    // Status bar / input
    let status = if let Some(s) = &app.status_message {
        s.clone()
    } else {
        match app.input_mode {
            InputMode::Search => format!("search: {}_", app.input),
            InputMode::LoginEmail => format!("email: {}_", app.input),
            InputMode::LoginPassword => format!("password: {}_", "*".repeat(app.input.len())),
            InputMode::ModuleEdit => {
                format!("{} = {}_", app.module_edit_key, app.module_edit_value)
            }
            _ => "[s] search  [d] downloads  [l] logs  [c] settings  [q] quit  [Tab] cycle tabs"
                .to_string(),
        }
    };
    let status_paragraph =
        Paragraph::new(status).block(Block::default().borders(Borders::ALL).title(" Status "));
    f.render_widget(status_paragraph, chunks[2]);
}

fn render_search(f: &mut Frame, app: &mut App, area: Rect) {
    let items: Vec<ListItem> = app
        .search_results
        .iter()
        .map(|r| {
            let title = r.name.clone().unwrap_or_else(|| r.result_id.clone());
            let artists = r.artists.clone().map(|v| v.join(", ")).unwrap_or_default();
            let dur = r
                .duration
                .map(|d| format!(" [{:02}:{:02}]", d / 60, d % 60))
                .unwrap_or_default();
            let line = Line::from(vec![
                Span::styled(title, Style::default().fg(Color::White)),
                Span::raw("  "),
                Span::styled(artists, Style::default().fg(Color::DarkGray)),
                Span::raw(dur),
            ]);
            ListItem::new(line)
        })
        .collect();
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Search Results "),
        )
        .highlight_style(Style::default().bg(Color::DarkGray).fg(Color::Cyan));
    f.render_stateful_widget(list, area, &mut app.search_state);
}

fn render_downloads(f: &mut Frame, app: &mut App, area: Rect) {
    let items: Vec<ListItem> = app
        .download_status
        .iter()
        .rev()
        .take(200)
        .map(|s| ListItem::new(s.clone()))
        .collect();
    let list = List::new(items).block(Block::default().borders(Borders::ALL).title(" Downloads "));
    f.render_stateful_widget(list, area, &mut app.download_state);
}

fn render_logs(f: &mut Frame, app: &mut App, area: Rect) {
    let items: Vec<ListItem> = app
        .log
        .iter()
        .rev()
        .take(500)
        .map(|(level, msg)| {
            let prefix = match level {
                LogLevel::Error => Span::styled("[ERR] ", Style::default().fg(Color::Red)),
                LogLevel::Warn => Span::styled("[WRN] ", Style::default().fg(Color::Yellow)),
                LogLevel::Info => Span::styled("[INF] ", Style::default().fg(Color::Green)),
            };
            ListItem::new(Line::from(vec![prefix, Span::raw(msg.clone())]))
        })
        .collect();
    let list = List::new(items).block(Block::default().borders(Borders::ALL).title(" Logs "));
    f.render_stateful_widget(list, area, &mut app.log_state);
}

fn render_settings(f: &mut Frame, app: &mut App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(area);
    // Left: section list
    let sections: Vec<ListItem> = vec![ListItem::new("General"), ListItem::new("Modules")];
    let list = List::new(sections)
        .block(Block::default().borders(Borders::ALL).title(" Sections "))
        .highlight_style(Style::default().bg(Color::DarkGray).fg(Color::Cyan));
    let mut state = ListState::default();
    state.select(Some(match app.settings_focus {
        SettingsFocus::General | SettingsFocus::ModuleDetail(_) => 0,
        SettingsFocus::Modules => 1,
    }));
    f.render_stateful_widget(list, chunks[0], &mut state);

    // Right: detail
    let right = match app.settings_focus {
        SettingsFocus::General => render_general_settings(app),
        SettingsFocus::Modules | SettingsFocus::ModuleDetail(_) => render_module_settings(app),
    };
    f.render_widget(right, chunks[1]);
}

fn render_general_settings(app: &App) -> Paragraph<'static> {
    let globals = &app.picaro.merged_globals;
    let mut lines: Vec<Line> = vec![Line::from(
        "Global settings (read-only - use the TUI Settings menu to edit)",
    )];
    if let Some(general) = globals.get("general").and_then(|v| v.as_object()) {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "[general]",
            Style::default().fg(Color::Cyan),
        )));
        for (k, v) in general {
            lines.push(Line::from(format!("  {k} = {v}")));
        }
    }
    if let Some(formatting) = globals.get("formatting").and_then(|v| v.as_object()) {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "[formatting]",
            Style::default().fg(Color::Cyan),
        )));
        for (k, v) in formatting {
            lines.push(Line::from(format!("  {k} = {v}")));
        }
    }
    if let Some(lyrics) = globals.get("lyrics").and_then(|v| v.as_object()) {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "[lyrics]",
            Style::default().fg(Color::Cyan),
        )));
        for (k, v) in lyrics {
            lines.push(Line::from(format!("  {k} = {v}")));
        }
    }
    Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" General "))
        .wrap(Wrap { trim: true })
}

fn render_module_settings(app: &App) -> Paragraph<'static> {
    let names = app.picaro.list_modules();
    let mut lines: Vec<Line> = vec![Line::from("Available modules (read-only)")];
    lines.push(Line::from(""));
    for (i, name) in names.iter().enumerate() {
        let marker = if i == app.module_focus { "▶" } else { " " };
        if let Some(m) = app.picaro.registry().get(name) {
            lines.push(Line::from(format!(
                "{} {} - {}",
                marker, m.information.service_name, name
            )));
        } else {
            lines.push(Line::from(format!("{marker} {name}")));
        }
    }
    lines.push(Line::from(""));
    if let Some(name) = names.get(app.module_focus) {
        if let Some(m) = app.picaro.registry().get(name) {
            lines.push(Line::from(Span::styled(
                format!("Settings for {}", m.information.service_name),
                Style::default().fg(Color::Cyan),
            )));
            if let Some(settings) = m.current_settings.as_ref() {
                for (k, v) in settings {
                    lines.push(Line::from(format!("  {k} = {v}")));
                }
            }
        }
    }
    Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" Modules "))
        .wrap(Wrap { trim: true })
}
