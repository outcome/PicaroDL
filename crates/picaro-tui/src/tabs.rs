//! The Tabs enumeration: which "page" of the TUI is currently shown.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Search,
    Downloads,
    Logs,
    Settings,
}

impl Tab {
    pub fn name(self) -> &'static str {
        match self {
            Tab::Search => "Search",
            Tab::Downloads => "Downloads",
            Tab::Logs => "Logs",
            Tab::Settings => "Settings",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Tab::Search => Tab::Downloads,
            Tab::Downloads => Tab::Logs,
            Tab::Logs => Tab::Settings,
            Tab::Settings => Tab::Search,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Tab::Search => Tab::Settings,
            Tab::Downloads => Tab::Search,
            Tab::Logs => Tab::Downloads,
            Tab::Settings => Tab::Logs,
        }
    }
}
