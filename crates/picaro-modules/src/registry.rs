//! Module registry.

use std::collections::HashMap;
use std::sync::Arc;

use once_cell::sync::Lazy;

use picaro_utils::error::Result;
use picaro_utils::models::ModuleInformation;
use picaro_utils::{ModuleConstructor, ModuleRegistry};

pub static MODULE_INFO: Lazy<HashMap<String, ModuleInformation>> = Lazy::new(|| {
    let mut m: HashMap<String, ModuleInformation> = HashMap::new();
    m.insert(
        "butterboy".to_string(),
        crate::butterboy::module_information(),
    );
    m.insert(
        "ccmixter".to_string(),
        crate::ccmixter::module_information(),
    );
    m.insert(
        "coreradio".to_string(),
        crate::coreradio::module_information(),
    );
    m.insert(
        "dance_music".to_string(),
        crate::dance_music::module_information(),
    );
    m.insert(
        "deezer_preview".to_string(),
        crate::deezer_preview::module_information(),
    );
    m.insert(
        "ektoplazm".to_string(),
        crate::ektoplazm::module_information(),
    );
    m.insert(
        "freemp3cloud".to_string(),
        crate::freemp3cloud::module_information(),
    );
    m.insert(
        "globaldjmix".to_string(),
        crate::globaldjmix::module_information(),
    );
    m.insert(
        "grimearchive".to_string(),
        crate::grimearchive::module_information(),
    );
    m.insert("lrclib".to_string(), crate::lrclib::module_information());
    m.insert(
        "lyrics_ovh".to_string(),
        crate::lyrics_ovh::module_information(),
    );
    m.insert("lyrist".to_string(), crate::lyrist::module_information());
    m.insert(
        "musixmatch".to_string(),
        crate::musixmatch::module_information(),
    );
    m.insert(
        "punkcata".to_string(),
        crate::punkcata::module_information(),
    );
    m.insert(
        "soundcloud".to_string(),
        crate::soundcloud::module_information(),
    );
    m.insert(
        "soulseek".to_string(),
        crate::soulseek::module_information(),
    );
    m.insert("tancpol".to_string(), crate::tancpol::module_information());
    m.insert("youtube".to_string(), crate::youtube::module_information());
    m.insert("zvu4it".to_string(), crate::zvu4it::module_information());
    m
});

pub fn register(
    registry: &ModuleRegistry,
    info: ModuleInformation,
    ctor: Arc<dyn ModuleConstructor>,
) {
    use picaro_utils::RegisteredModule;
    let rm = RegisteredModule {
        name: info.service_name.clone(),
        information: info,
        current_settings: None,
        constructor: ctor,
    };
    registry.insert(&rm.name.to_lowercase(), rm);
}

pub fn register_all(registry: &ModuleRegistry) -> Result<()> {
    crate::butterboy::register_module(registry);
    crate::ccmixter::register_module(registry);
    crate::coreradio::register_module(registry);
    crate::dance_music::register_module(registry);
    crate::deezer_preview::register_module(registry);
    crate::ektoplazm::register_module(registry);
    crate::freemp3cloud::register_module(registry);
    crate::globaldjmix::register_module(registry);
    crate::grimearchive::register_module(registry);
    crate::lrclib::register_module(registry);
    crate::lyrics_ovh::register_module(registry);
    crate::lyrist::register_module(registry);
    crate::musixmatch::register_module(registry);
    crate::punkcata::register_module(registry);
    crate::soundcloud::register_module(registry);
    crate::soulseek::register_module(registry);
    crate::tancpol::register_module(registry);
    crate::youtube::register_module(registry);
    crate::zvu4it::register_module(registry);
    Ok(())
}
