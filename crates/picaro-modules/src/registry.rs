//! Module registry: collects ModuleInformation for every bundled module
//! and provides helpers to register them all into the ModuleRegistry.

use std::collections::HashMap;
use std::sync::Arc;

use once_cell::sync::Lazy;

use picaro_utils::error::Result;
use picaro_utils::models::ModuleInformation;
use picaro_utils::{ModuleConstructor, ModuleRegistry};

pub static MODULE_INFO: Lazy<HashMap<String, ModuleInformation>> = Lazy::new(|| {
    let mut m: HashMap<String, ModuleInformation> = HashMap::new();
    m.insert(
        "aboutdiscowithlove".to_string(),
        crate::aboutdiscowithlove::module_information(),
    );
    m.insert(
        "alterportal".to_string(),
        crate::alterportal::module_information(),
    );
    m.insert(
        "archive_org".to_string(),
        crate::archive_org::module_information(),
    );
    m.insert(
        "burningtheground".to_string(),
        crate::burningtheground::module_information(),
    );
    m.insert(
        "butterboy".to_string(),
        crate::butterboy::module_information(),
    );
    m.insert(
        "ccmixter".to_string(),
        crate::ccmixter::module_information(),
    );
    m.insert(
        "classicalmusicdownload".to_string(),
        crate::classicalmusicdownload::module_information(),
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
        "deadpulpit".to_string(),
        crate::deadpulpit::module_information(),
    );
    m.insert(
        "dieordiy2".to_string(),
        crate::dieordiy2::module_information(),
    );
    m.insert("discogc".to_string(), crate::discogc::module_information());
    m.insert(
        "discografias".to_string(),
        crate::discografias::module_information(),
    );
    m.insert(
        "edmwaves".to_string(),
        crate::edmwaves::module_information(),
    );
    m.insert(
        "ektoplazm".to_string(),
        crate::ektoplazm::module_information(),
    );
    m.insert(
        "essentialhouse".to_string(),
        crate::essentialhouse::module_information(),
    );
    m.insert(
        "exystence".to_string(),
        crate::exystence::module_information(),
    );
    m.insert(
        "ezhevika".to_string(),
        crate::ezhevika::module_information(),
    );
    m.insert(
        "flacmusic".to_string(),
        crate::flacmusic::module_information(),
    );
    m.insert(
        "flatblackandclassical".to_string(),
        crate::flatblackandclassical::module_information(),
    );
    m.insert(
        "flights1000".to_string(),
        crate::flights1000::module_information(),
    );
    m.insert(
        "foggynotionflac".to_string(),
        crate::foggynotionflac::module_information(),
    );
    m.insert(
        "fondsound".to_string(),
        crate::fondsound::module_information(),
    );
    m.insert(
        "forwardwiththesong".to_string(),
        crate::forwardwiththesong::module_information(),
    );
    m.insert("fpftp".to_string(), crate::fpftp::module_information());
    m.insert(
        "freemp3cloud".to_string(),
        crate::freemp3cloud::module_information(),
    );
    m.insert(
        "globaldjmix".to_string(),
        crate::globaldjmix::module_information(),
    );
    m.insert(
        "glorybeats".to_string(),
        crate::glorybeats::module_information(),
    );
    m.insert(
        "goldhiphop".to_string(),
        crate::goldhiphop::module_information(),
    );
    m.insert(
        "grimearchive".to_string(),
        crate::grimearchive::module_information(),
    );
    m.insert(
        "hiphop94".to_string(),
        crate::hiphop94::module_information(),
    );
    m.insert("hiphopa".to_string(), crate::hiphopa::module_information());
    m.insert(
        "hipstrumentals".to_string(),
        crate::hipstrumentals::module_information(),
    );
    m.insert(
        "inconstantsol".to_string(),
        crate::inconstantsol::module_information(),
    );
    m.insert(
        "intmusic".to_string(),
        crate::intmusic::module_information(),
    );
    m.insert(
        "iplusfree".to_string(),
        crate::iplusfree::module_information(),
    );
    m.insert(
        "josephbarulho".to_string(),
        crate::josephbarulho::module_information(),
    );
    m.insert(
        "kpopdownloadscmm".to_string(),
        crate::kpopdownloadscmm::module_information(),
    );
    m.insert(
        "ktimusic".to_string(),
        crate::ktimusic::module_information(),
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
        "lostsongshc".to_string(),
        crate::lostsongshc::module_information(),
    );
    m.insert("lrclib".to_string(), crate::lrclib::module_information());
    m.insert(
        "lyrics_ovh".to_string(),
        crate::lyrics_ovh::module_information(),
    );
    m.insert("lyrist".to_string(), crate::lyrist::module_information());
    m.insert(
        "madrotter".to_string(),
        crate::madrotter::module_information(),
    );
    m.insert(
        "meetinginmusic".to_string(),
        crate::meetinginmusic::module_information(),
    );
    m.insert(
        "metalminos".to_string(),
        crate::metalminos::module_information(),
    );
    m.insert("mp3db".to_string(), crate::mp3db::module_information());
    m.insert(
        "musicrepublicworld".to_string(),
        crate::musicrepublicworld::module_information(),
    );
    m.insert(
        "musicrider".to_string(),
        crate::musicrider::module_information(),
    );
    m.insert("musify".to_string(), crate::musify::module_information());
    m.insert(
        "musixmatch".to_string(),
        crate::musixmatch::module_information(),
    );
    m.insert(
        "nathannothinsez".to_string(),
        crate::nathannothinsez::module_information(),
    );
    m.insert(
        "newalbumreleases".to_string(),
        crate::newalbumreleases::module_information(),
    );
    m.insert("nodata".to_string(), crate::nodata::module_information());
    m.insert(
        "nuclearholocaust".to_string(),
        crate::nuclearholocaust::module_information(),
    );
    m.insert(
        "oldschoolmetalmusicz".to_string(),
        crate::oldschoolmetalmusicz::module_information(),
    );
    m.insert(
        "onetechno".to_string(),
        crate::onetechno::module_information(),
    );
    m.insert(
        "onetrance".to_string(),
        crate::onetrance::module_information(),
    );
    m.insert(
        "paradiseofgaragecomps".to_string(),
        crate::paradiseofgaragecomps::module_information(),
    );
    m.insert(
        "primitiveofferings".to_string(),
        crate::primitiveofferings::module_information(),
    );
    m.insert(
        "progrockvintage".to_string(),
        crate::progrockvintage::module_information(),
    );
    m.insert("psyfp".to_string(), crate::psyfp::module_information());
    m.insert(
        "punkcata".to_string(),
        crate::punkcata::module_information(),
    );
    m.insert("rapload".to_string(), crate::rapload::module_information());
    m.insert(
        "rapwarfam".to_string(),
        crate::rapwarfam::module_information(),
    );
    m.insert(
        "sophiesfloorboard".to_string(),
        crate::sophiesfloorboard::module_information(),
    );
    m.insert(
        "soundclick".to_string(),
        crate::soundclick::module_information(),
    );
    m.insert(
        "soundcloud".to_string(),
        crate::soundcloud::module_information(),
    );
    m.insert(
        "systemsofromance".to_string(),
        crate::systemsofromance::module_information(),
    );
    m.insert(
        "takemetal".to_string(),
        crate::takemetal::module_information(),
    );
    m.insert("tancpol".to_string(), crate::tancpol::module_information());
    m.insert(
        "tapeattack".to_string(),
        crate::tapeattack::module_information(),
    );
    m.insert(
        "technicaldeathmetal".to_string(),
        crate::technicaldeathmetal::module_information(),
    );
    m.insert(
        "themfire".to_string(),
        crate::themfire::module_information(),
    );
    m.insert(
        "tomlehrersongs".to_string(),
        crate::tomlehrersongs::module_information(),
    );
    m.insert(
        "toneandwave".to_string(),
        crate::toneandwave::module_information(),
    );
    m.insert(
        "tsquareplaza".to_string(),
        crate::tsquareplaza::module_information(),
    );
    m.insert(
        "urbanaspirines".to_string(),
        crate::urbanaspirines::module_information(),
    );
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
    crate::aboutdiscowithlove::register_module(registry);
    crate::alterportal::register_module(registry);
    crate::archive_org::register_module(registry);
    crate::burningtheground::register_module(registry);
    crate::butterboy::register_module(registry);
    crate::ccmixter::register_module(registry);
    crate::classicalmusicdownload::register_module(registry);
    crate::coreradio::register_module(registry);
    crate::dance_music::register_module(registry);
    crate::deadpulpit::register_module(registry);
    crate::dieordiy2::register_module(registry);
    crate::discogc::register_module(registry);
    crate::discografias::register_module(registry);
    crate::edmwaves::register_module(registry);
    crate::ektoplazm::register_module(registry);
    crate::essentialhouse::register_module(registry);
    crate::exystence::register_module(registry);
    crate::ezhevika::register_module(registry);
    crate::flacmusic::register_module(registry);
    crate::flatblackandclassical::register_module(registry);
    crate::flights1000::register_module(registry);
    crate::foggynotionflac::register_module(registry);
    crate::fondsound::register_module(registry);
    crate::forwardwiththesong::register_module(registry);
    crate::fpftp::register_module(registry);
    crate::freemp3cloud::register_module(registry);
    crate::globaldjmix::register_module(registry);
    crate::glorybeats::register_module(registry);
    crate::goldhiphop::register_module(registry);
    crate::grimearchive::register_module(registry);
    crate::hiphop94::register_module(registry);
    crate::hiphopa::register_module(registry);
    crate::hipstrumentals::register_module(registry);
    crate::inconstantsol::register_module(registry);
    crate::intmusic::register_module(registry);
    crate::iplusfree::register_module(registry);
    crate::josephbarulho::register_module(registry);
    crate::kpopdownloadscmm::register_module(registry);
    crate::ktimusic::register_module(registry);
    crate::losslessalbums::register_module(registry);
    crate::losslessmusic::register_module(registry);
    crate::lostsongshc::register_module(registry);
    crate::lrclib::register_module(registry);
    crate::lyrics_ovh::register_module(registry);
    crate::lyrist::register_module(registry);
    crate::madrotter::register_module(registry);
    crate::meetinginmusic::register_module(registry);
    crate::metalminos::register_module(registry);
    crate::mp3db::register_module(registry);
    crate::musicrepublicworld::register_module(registry);
    crate::musicrider::register_module(registry);
    crate::musify::register_module(registry);
    crate::musixmatch::register_module(registry);
    crate::nathannothinsez::register_module(registry);
    crate::newalbumreleases::register_module(registry);
    crate::nodata::register_module(registry);
    crate::nuclearholocaust::register_module(registry);
    crate::oldschoolmetalmusicz::register_module(registry);
    crate::onetechno::register_module(registry);
    crate::onetrance::register_module(registry);
    crate::paradiseofgaragecomps::register_module(registry);
    crate::primitiveofferings::register_module(registry);
    crate::progrockvintage::register_module(registry);
    crate::psyfp::register_module(registry);
    crate::punkcata::register_module(registry);
    crate::rapload::register_module(registry);
    crate::rapwarfam::register_module(registry);
    crate::sophiesfloorboard::register_module(registry);
    crate::soundclick::register_module(registry);
    crate::soundcloud::register_module(registry);
    crate::systemsofromance::register_module(registry);
    crate::takemetal::register_module(registry);
    crate::tancpol::register_module(registry);
    crate::tapeattack::register_module(registry);
    crate::technicaldeathmetal::register_module(registry);
    crate::themfire::register_module(registry);
    crate::tomlehrersongs::register_module(registry);
    crate::toneandwave::register_module(registry);
    crate::tsquareplaza::register_module(registry);
    crate::urbanaspirines::register_module(registry);
    crate::youtube::register_module(registry);
    crate::zvu4it::register_module(registry);
    crate::stubs::register_all_stubs(registry);
    Ok(())
}
