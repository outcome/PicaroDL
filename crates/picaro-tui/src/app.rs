//! Top-level TUI application: handles key events, draws the screen, owns
//! the `Downloader` and `Picaro` core.
//!
//! Search fans out to every download-capable module in parallel and merges
//! results live; Enter downloads the selected result; the Settings tab
//! edits `config/settings.json` in place (restart applies changes).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use serde_json::Value;

use picaro_core::Picaro;
use picaro_downloader::{DownloadEvent, Downloader, LogLevel};
use picaro_utils::models::{ModuleModes, ModuleFlags, SearchResult};
use picaro_utils::settings as settings_io;

use crate::tabs::Tab;

/// Messages from spawned tasks back to the UI loop.
pub enum UiMsg {
    Results(Vec<(String, SearchResult)>),
    Status(String),
}

pub struct App {
    pub picaro: Arc<Picaro>,
    pub downloader: Arc<Downloader>,
    pub tab: Tab,
    pub input: String,
    pub search_results: Vec<(String, SearchResult)>,
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
    pub settings_cursor: usize,
    pub module_focus: usize,
    pub module_edit_key: String,
    pub inbox: Arc<Mutex<Vec<UiMsg>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    None,
    Search,
    Settings,
    ModuleEdit,
    Confirm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsFocus {
    General,
    Modules,
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
            // Start in normal mode so single-key shortcuts (s/q/Tab/...)
            // work immediately; the search box is one `s` away.
            input_mode: InputMode::None,
            quality_choice: 4, // default to hifi
            settings_focus: SettingsFocus::General,
            settings_cursor: 0,
            module_focus: 0,
            module_edit_key: String::new(),
            inbox: Arc::new(Mutex::new(Vec::new())),
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
            InputMode::Settings => self.on_key_settings(key),
            InputMode::ModuleEdit => self.on_key_edit(key),
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
                self.input.clear();
                self.input_mode = InputMode::Search;
                self.tab = Tab::Search;
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
            KeyCode::Enter => self.download_selected(),
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            _ => {}
        }
    }

    fn move_selection(&mut self, delta: i32) {
        if self.search_results.is_empty() {
            return;
        }
        let len = self.search_results.len();
        let i = self.search_state.selected().unwrap_or(0);
        let next = if delta > 0 {
            (i + 1).min(len - 1)
        } else {
            i.saturating_sub(1)
        };
        self.search_state.select(Some(next));
    }

    /// Download the currently selected search result (spawned so the UI
    /// keeps responding; progress arrives via download events).
    fn download_selected(&mut self) {
        let Some(i) = self.search_state.selected() else {
            return;
        };
        let Some((service, result)) = self.search_results.get(i) else {
            return;
        };
        let (service, result) = (service.clone(), result.clone());
        let name = result.name.clone().unwrap_or_else(|| result.result_id.clone());
        self.status_message = Some(format!("Downloading {name} via {service}..."));
        let downloader = self.downloader.clone();
        tokio::spawn(async move {
            let mut data = HashMap::new();
            data.insert(
                "__track_name__".to_string(),
                Value::String(result.name.clone().unwrap_or_default()),
            );
            if let Some(a) = result.artists.as_ref().and_then(|v| v.first()) {
                data.insert("__artist__".to_string(), Value::String(a.clone()));
            }
            let res = downloader
                .download_track_with_data(&service, &result.result_id, data)
                .await;
            if let Err(e) = res {
                let _ = e; // failure arrives via the download event channel
            }
        });
    }

    fn on_key_search(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.input_mode = InputMode::None;
                self.status_message = None;
            }
            KeyCode::Enter if self.input.trim().is_empty() => {
                self.input_mode = InputMode::None;
                self.status_message = None;
            }
            KeyCode::Enter => {
                let q = self.input.trim().to_string();
                self.input.clear();
                self.input_mode = InputMode::None;
                self.search_results.clear();
                self.search_state.select(None);
                self.status_message = Some(format!("Searching all modules for '{q}'..."));
                spawn_search_all(
                    self.picaro.clone(),
                    self.downloader.clone(),
                    self.inbox.clone(),
                    q,
                );
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
                };
            }
            KeyCode::Char('q') => {
                self.input_mode = InputMode::None;
            }
            KeyCode::Up => match self.settings_focus {
                SettingsFocus::General => {
                    self.settings_cursor = self.settings_cursor.saturating_sub(1);
                }
                SettingsFocus::Modules => {
                    if self.module_focus > 0 {
                        self.module_focus -= 1;
                    }
                }
            },
            KeyCode::Down => match self.settings_focus {
                SettingsFocus::General => {
                    let rows = general_settings_rows(&self.picaro);
                    if self.settings_cursor + 1 < rows.len().max(1) {
                        self.settings_cursor += 1;
                    }
                }
                SettingsFocus::Modules => {
                    let module_count = self.picaro.list_modules().len();
                    if self.module_focus + 1 < module_count {
                        self.module_focus += 1;
                    }
                }
            },
            KeyCode::Enter => {
                if let SettingsFocus::General = self.settings_focus {
                    let rows = general_settings_rows(&self.picaro);
                    if let Some((section, key, value)) = rows.get(self.settings_cursor) {
                        self.module_edit_key = format!("{section}.{key}");
                        self.input = match value {
                            Value::String(s) => s.clone(),
                            v => v.to_string(),
                        };
                        self.input_mode = InputMode::ModuleEdit;
                        self.status_message =
                            Some("Type the new value, Enter to save, Esc to cancel".into());
                    }
                }
            }
            _ => {}
        }
    }

    fn on_key_edit(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.input_mode = InputMode::Settings;
                self.input.clear();
                self.status_message = None;
            }
            KeyCode::Enter => {
                let target = self.module_edit_key.clone();
                let text = self.input.clone();
                self.input.clear();
                self.input_mode = InputMode::Settings;
                self.status_message = Some(match save_setting(&self.picaro, &target, &text) {
                    Ok(path) => format!(
                        "Saved {} = {text} to {} (restart to apply)",
                        target,
                        path.display()
                    ),
                    Err(e) => format!("Save failed: {e}"),
                });
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
}

/// Registry names of every module that can download, in registration order.
fn download_services(picaro: &Picaro) -> Vec<String> {
    let mut out = Vec::new();
    for name in picaro.list_modules() {
        if let Some(m) = picaro.registry().get(&name) {
            let i = &m.information;
            if i.flags.contains(ModuleFlags::hidden)
                || !i.module_supported_modes.contains(ModuleModes::download)
            {
                continue;
            }
            out.push(name);
        }
    }
    out
}

/// Fan a search out to every download-capable module in parallel; each
/// finished module pushes its results into the UI inbox, so the list
/// grows live instead of blocking on the slowest source.
fn spawn_search_all(
    picaro: Arc<Picaro>,
    downloader: Arc<Downloader>,
    inbox: Arc<Mutex<Vec<UiMsg>>>,
    query: String,
) {
    tokio::spawn(async move {
        let services = download_services(&picaro);
        let total = services.len();
        let answered = Arc::new(Mutex::new(0usize));
        let mut handles = Vec::new();
        for service in services {
            let downloader = downloader.clone();
            let inbox = inbox.clone();
            let query = query.clone();
            let answered = answered.clone();
            handles.push(tokio::spawn(async move {
                let res = downloader
                    .search(
                        &service,
                        &query,
                        picaro_utils::models::DownloadType::track,
                    )
                    .await;
                let mut a = answered.lock().unwrap();
                *a += 1;
                let done = *a;
                match res {
                    Ok(results) => {
                        let n = results.len();
                        inbox.lock().unwrap().push(UiMsg::Results(
                            results.into_iter().map(|r| (service.clone(), r)).collect(),
                        ));
                        inbox
                            .lock()
                            .unwrap()
                            .push(UiMsg::Status(format!(
                                "{done}/{total} modules answered ({n} from {service})"
                            )));
                    }
                    Err(e) => {
                        inbox
                            .lock()
                            .unwrap()
                            .push(UiMsg::Status(format!(
                                "{done}/{total} modules answered ({service}: {e})"
                            )));
                    }
                }
            }));
        }
        for h in handles {
            let _ = h.await;
        }
        inbox
            .lock()
            .unwrap()
            .push(UiMsg::Status(format!("Search finished ({total} modules queried)")));
    });
}

/// Flattened `[section] key = value` rows from the merged globals.
fn general_settings_rows(picaro: &Picaro) -> Vec<(String, String, Value)> {
    let mut rows = Vec::new();
    for (section, value) in picaro.merged_globals.iter() {
        if let Some(obj) = value.as_object() {
            for (k, v) in obj {
                rows.push((section.clone(), k.clone(), v.clone()));
            }
        }
    }
    rows
}

/// Write one `section.key = value` into `settings.json`. Values parse as
/// JSON when possible ("true", "5", "1.5") and as strings otherwise.
/// The change is persisted; a restart applies it.
fn save_setting(
    picaro: &Picaro,
    target: &str,
    text: &str,
) -> std::io::Result<std::path::PathBuf> {
    let Some((section, key)) = target.split_once('.') else {
        return Err(std::io::Error::other("setting key missing section"));
    };
    let mut doc = settings_io::load_settings(&picaro.data_folder);
    let global = doc
        .entry("global".to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let Some(obj) = global.as_object_mut() else {
        return Err(std::io::Error::other("settings.json [global] is not an object"));
    };
    let section_obj = obj
        .entry(section.to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let Some(section_obj) = section_obj.as_object_mut() else {
        return Err(std::io::Error::other(format!(
            "settings.json [global.{section}] is not an object"
        )));
    };
    let value: Value = serde_json::from_str(text).unwrap_or(Value::String(text.to_string()));
    section_obj.insert(key.to_string(), value);
    settings_io::save_settings(&picaro.data_folder, &doc)?;
    Ok(settings_io::settings_path(&picaro.data_folder))
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
    // A relative download_path used to resolve against the process CWD,
    // scattering downloads across whatever directory the binary was started
    // from. Anchor relative paths to the project root (config/ sits inside
    // it) instead.
    let download_path = if download_path.is_absolute() {
        download_path
    } else {
        let root = picaro_core::loader::project_root()
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        root.join(download_path)
    };
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

/// Merge messages from spawned search tasks into the visible state.
fn drain_inbox(app: &mut App) {
    let msgs: Vec<UiMsg> = std::mem::take(&mut *app.inbox.lock().unwrap());
    for msg in msgs {
        match msg {
            UiMsg::Results(items) => {
                let selected = app.search_state.selected().unwrap_or(0);
                for (service, r) in items {
                    if app.search_results.len() >= 150 {
                        break;
                    }
                    if app
                        .search_results
                        .iter()
                        .any(|(_, existing)| existing.result_id == r.result_id)
                    {
                        continue;
                    }
                    app.search_results.push((service, r));
                }
                if app.search_state.selected().is_none() && !app.search_results.is_empty() {
                    app.search_state.select(Some(0));
                }
                let _ = selected;
            }
            UiMsg::Status(text) => {
                app.status_message = Some(text);
            }
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
        drain_inbox(app);
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
    let status = if let InputMode::Search = app.input_mode {
        format!("search: {}_", app.input)
    } else if let InputMode::ModuleEdit = app.input_mode {
        format!("set {} = {}_  (Enter saves, Esc cancels)", app.module_edit_key, app.input)
    } else if let Some(s) = &app.status_message {
        s.clone()
    } else {
        "[s] search  [d] downloads  [l] logs  [c] settings  [q] quit  [Tab] cycle tabs"
            .to_string()
    };
    let status_paragraph =
        Paragraph::new(status).block(Block::default().borders(Borders::ALL).title(" Status "));
    f.render_widget(status_paragraph, chunks[2]);
}

fn render_search(f: &mut Frame, app: &mut App, area: Rect) {
    let items: Vec<ListItem> = app
        .search_results
        .iter()
        .map(|(service, r)| {
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
                Span::styled(format!("  ({service})"), Style::default().fg(Color::DarkGray)),
            ]);
            ListItem::new(line)
        })
        .collect();
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Search Results (Enter to download) "),
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
        SettingsFocus::General => 0,
        SettingsFocus::Modules => 1,
    }));
    f.render_stateful_widget(list, chunks[0], &mut state);

    // Right: detail
    let right = match app.settings_focus {
        SettingsFocus::General => render_general_settings(app),
        SettingsFocus::Modules => render_module_settings(app),
    };
    f.render_widget(right, chunks[1]);
}

fn render_general_settings(app: &App) -> Paragraph<'static> {
    let rows = general_settings_rows(&app.picaro);
    let mut lines: Vec<Line> = vec![Line::from(Span::styled(
        "Up/Down to pick, Enter to edit, Esc to leave",
        Style::default().fg(Color::DarkGray),
    ))];
    let mut current_section = String::new();
    for (i, (section, key, value)) in rows.iter().enumerate() {
        if *section != current_section {
            current_section = section.clone();
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("[{section}]"),
                Style::default().fg(Color::Cyan),
            )));
        }
        let marker = if i == app.settings_cursor { "▶" } else { " " };
        let style = if i == app.settings_cursor {
            Style::default().bg(Color::DarkGray).fg(Color::Cyan)
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(
            format!("{marker} {key} = {value}"),
            style,
        )));
    }
    if rows.is_empty() {
        lines.push(Line::from("(no settings found)"));
    }
    Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" General "))
        .wrap(Wrap { trim: true })
}

fn render_module_settings(app: &App) -> Paragraph<'static> {
    let names = app.picaro.list_modules();
    let mut lines: Vec<Line> = vec![Line::from(Span::styled(
        "Modules registered (module-specific settings live in settings.json)",
        Style::default().fg(Color::DarkGray),
    ))];
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
