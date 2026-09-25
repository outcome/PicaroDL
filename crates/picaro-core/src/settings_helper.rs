//! Temporary settings controller wrappers - kept thin here so we can put
//! concrete convenience methods on the model in `picaro-utils` later.

use std::path::Path;

use picaro_utils::error::Result;
use picaro_utils::models::{ModuleController, TempSettingType};
use picaro_utils::settings as settings_io;
use serde_json::Value;

pub fn tsc_read(
    controller: &ModuleController,
    setting: &str,
    setting_type: TempSettingType,
) -> Result<Option<Value>> {
    settings_io::read_temporary_setting(
        &controller.temporary_settings_controller.session_path,
        &controller.temporary_settings_controller.module,
        setting,
        setting_type,
    )
}

pub fn tsc_set(
    controller: &ModuleController,
    setting: &str,
    value: Value,
    setting_type: TempSettingType,
) -> Result<()> {
    settings_io::set_temporary_setting(
        &controller.temporary_settings_controller.session_path,
        &controller.temporary_settings_controller.module,
        setting,
        value,
        setting_type,
    )
}
