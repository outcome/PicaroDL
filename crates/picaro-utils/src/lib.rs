//! The PicaroDL public library - re-exports everything that's stable.

pub mod error;
pub mod error_simplify;
pub mod format;
pub mod http;
pub mod metadata_fill;
pub mod models;
pub mod module;
pub mod quality;
pub mod safety;
pub mod settings;
pub mod textmatch;
pub mod url_decode;
pub mod util;

pub use error::{Error, Result};

use std::sync::Arc;

use indexmap::IndexMap;
use parking_lot::RwLock;

/// In-memory registry of the modules the loader has discovered. The keys are
/// the lowercased module name.
#[derive(Debug, Clone, Default)]
pub struct ModuleRegistry {
    inner: Arc<RwLock<IndexMap<String, RegisteredModule>>>,
}

/// A registered module's metadata plus its current effective settings.
#[derive(Debug, Clone)]
pub struct RegisteredModule {
    pub name: String,
    pub information: models::ModuleInformation,
    pub current_settings: Option<serde_json::Map<String, serde_json::Value>>,
    pub constructor: Arc<dyn ModuleConstructor>,
}

/// Type alias for the trait object that every module exposes.
pub type ModuleInterfacePtr = Arc<dyn crate::module::ModuleInterface>;

impl ModuleRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, name: &str, rm: RegisteredModule) {
        let mut inner = self.inner.write();
        inner.insert(name.to_lowercase(), rm);
    }

    pub fn remove(&self, name: &str) -> Option<RegisteredModule> {
        let mut inner = self.inner.write();
        inner.shift_remove(&name.to_lowercase())
    }

    pub fn get(&self, name: &str) -> Option<RegisteredModule> {
        let inner = self.inner.read();
        inner.get(&name.to_lowercase()).cloned()
    }

    pub fn iter(&self) -> Vec<RegisteredModule> {
        let inner = self.inner.read();
        inner.values().cloned().collect()
    }

    pub fn names(&self) -> Vec<String> {
        let inner = self.inner.read();
        inner.keys().cloned().collect()
    }
}

/// Trait that every module crate implements to construct its `ModuleInterface`
/// when the loader asks for it.
pub trait ModuleConstructor: Send + Sync + std::fmt::Debug {
    fn construct(
        &self,
        controller: models::ModuleController,
    ) -> Result<Arc<dyn crate::module::ModuleInterface>>;
}
