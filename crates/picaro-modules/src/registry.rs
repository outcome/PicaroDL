//! Module registry: collects `ModuleInformation` for every bundled module
//! and provides helpers to register them all into the `ModuleRegistry`.

use std::collections::HashMap;
use std::sync::Arc;

use once_cell::sync::Lazy;

use picaro_utils::error::Result;
use picaro_utils::models::ModuleInformation;
use picaro_utils::{ModuleConstructor, ModuleRegistry};

pub static MODULE_INFO: Lazy<HashMap<String, ModuleInformation>> = Lazy::new(|| {
    let mut m: HashMap<String, ModuleInformation> = HashMap::new();
    m.insert("qobuz".to_string(), crate::qobuz::module_information());
    m.insert("tidal".to_string(), crate::tidal::module_information());
    m.insert("deezer".to_string(), crate::deezer::module_information());
    m.insert("youtube".to_string(), crate::youtube::module_information());
    m.insert("lrclib".to_string(), crate::lrclib::module_information());
    m.insert(
        "musixmatch".to_string(),
        crate::musixmatch::module_information(),
    );
    m.insert(
        "soundcloud".to_string(),
        crate::soundcloud::module_information(),
    );
    m.insert(
        "beatport".to_string(),
        crate::beatport::module_information(),
    );
    m.insert(
        "beatsource".to_string(),
        crate::beatsource::module_information(),
    );
    m.insert(
        "coreradio".to_string(),
        crate::coreradio::module_information(),
    );
    m.insert(
        "alterportal".to_string(),
        crate::alterportal::module_information(),
    );
    m.insert(
        "exystence".to_string(),
        crate::exystence::module_information(),
    );
    m.insert("spotify".to_string(), crate::spotify::module_information());
    m.insert("mp3db".to_string(), crate::mp3db::module_information());
    m.insert(
        "themfire".to_string(),
        crate::themfire::module_information(),
    );
    m.insert(
        "flacmusic".to_string(),
        crate::flacmusic::module_information(),
    );
    m.insert(
        "losslessalbums".to_string(),
        crate::losslessalbums::module_information(),
    );
    m.insert(
        "losslessmusic".to_string(),
        crate::losslessmusic::module_information(),
    );
    m.insert(
        "newalbumreleases".to_string(),
        crate::newalbumreleases::module_information(),
    );
    m.insert(
        "punkcata".to_string(),
        crate::punkcata::module_information(),
    );
    m.insert(
        "ezhevika".to_string(),
        crate::ezhevika::module_information(),
    );
    m.insert(
        "butterboy".to_string(),
        crate::butterboy::module_information(),
    );
    m.insert(
        "primitiveofferings".to_string(),
        crate::primitiveofferings::module_information(),
    );
    m.insert(
        "archiveorg".to_string(),
        crate::archive_org::module_information(),
    );
    m.insert("musify".to_string(), crate::musify::module_information());
    m.insert(
        "ccmixter".to_string(),
        crate::ccmixter::module_information(),
    );
    m.insert(
        "intmusic".to_string(),
        crate::intmusic::module_information(),
    );
    m.insert(
        "musicrider".to_string(),
        crate::musicrider::module_information(),
    );
    m.insert(
        "glorybeats".to_string(),
        crate::glorybeats::module_information(),
    );
    m.insert(
        "discografias".to_string(),
        crate::discografias::module_information(),
    );
    m.insert("discogc".to_string(), crate::discogc::module_information());
    m.insert(
        "deadpulpit".to_string(),
        crate::deadpulpit::module_information(),
    );
    m.insert(
        "iplusfree".to_string(),
        crate::iplusfree::module_information(),
    );
    m.insert(
        "soundclick".to_string(),
        crate::soundclick::module_information(),
    );
    m.insert("zvu4it".to_string(), crate::zvu4it::module_information());
    m.insert("tancpol".to_string(), crate::tancpol::module_information());
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
    // DISABLED (download requires sign-in): qobuz, tidal, deezer
    crate::youtube::register_module(registry);
    crate::lrclib::register_module(registry);
    crate::musixmatch::register_module(registry);
    crate::soundcloud::register_module(registry);
    // DISABLED (download requires sign-in): beatport, beatsource
    crate::coreradio::register_module(registry);
    crate::alterportal::register_module(registry);
    crate::exystence::register_module(registry);
    // DISABLED (download requires sign-in): spotify
    crate::mp3db::register_module(registry);
    crate::themfire::register_module(registry);
    crate::flacmusic::register_module(registry);
    crate::losslessalbums::register_module(registry);
    // DISABLED (Cloudflare 403): losslessmusic, newalbumreleases
    // DISABLED (host-level 403): musify
    crate::punkcata::register_module(registry);
    crate::ezhevika::register_module(registry);
    crate::butterboy::register_module(registry);
    crate::primitiveofferings::register_module(registry);
    // DISABLED (poor relevance / mixed formats): archiveorg
    crate::ccmixter::register_module(registry);
    crate::musicrider::register_module(registry);
    // DISABLED (Cloudflare-walled download - skipped by design, no cookie UX):
    //   intmusic, discogc, glorybeats, losslessmusic, newalbumreleases
    // DISABLED (shortener-only links, ~no coverage): discografias
    crate::deadpulpit::register_module(registry);
    crate::iplusfree::register_module(registry);
    // DISABLED (search returns 0 / broken): soundclick
    crate::zvu4it::register_module(registry);
    crate::tancpol::register_module(registry);
    // DISABLED (placeholder stubs): crate::stubs::register_all_stubs(registry);
    Ok(())
}
