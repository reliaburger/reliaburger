//! The cluster's `[appliance]` settings (#2151): choices about appliances
//! that the whole cluster makes once, in the council, rather than each
//! machine in its own `node.toml`.
//!
//! There's one so far, `console_chaos`, which turns on the console
//! invaders' kill mode. It has to be a council setting: a switch in a
//! node's own files is a switch for whoever stands at that node's
//! keyboard, and kill mode must never be that.
//!
//! The settings live in the council's key-value map
//! (`DesiredState::config`, written with `RaftRequest::ConfigSet`), one key
//! per field, so adding one needs no new Raft request or state format.
//! This module is the only code that names those keys.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// The council key that holds [`ApplianceSettings::console_chaos`].
pub const CONSOLE_CHAOS_KEY: &str = "appliance.console_chaos";

/// The cluster's `[appliance]` section.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplianceSettings {
    /// Whether a hit in the console's invaders game kills the instance the
    /// invader stands for, as a Smoker fault. Off unless set.
    #[serde(default)]
    pub console_chaos: bool,
}

/// Why a settings file couldn't be read.
#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("not valid TOML: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("no [appliance] section")]
    NoSection,
}

/// A file holding the section, and perhaps others, which are ignored, so a
/// cluster's whole settings file can be given.
#[derive(Deserialize)]
struct SettingsFile {
    appliance: Option<ApplianceSettings>,
}

impl ApplianceSettings {
    /// The settings the council's key-value map holds. A key that's
    /// missing, or holds anything but `true`, reads as off.
    pub fn from_council(config: &HashMap<String, String>) -> Self {
        Self {
            console_chaos: config.get(CONSOLE_CHAOS_KEY).map(String::as_str) == Some("true"),
        }
    }

    /// The council keys and values that store these settings.
    pub fn council_entries(&self) -> Vec<(String, String)> {
        vec![(
            CONSOLE_CHAOS_KEY.to_string(),
            self.console_chaos.to_string(),
        )]
    }

    /// The `[appliance]` section of a TOML file.
    ///
    /// ```
    /// use reliaburger::appliance::settings::ApplianceSettings;
    ///
    /// let settings = ApplianceSettings::from_toml("[appliance]\nconsole_chaos = true\n").unwrap();
    /// assert!(settings.console_chaos);
    /// ```
    pub fn from_toml(text: &str) -> Result<Self, SettingsError> {
        let file: SettingsFile = toml::from_str(text)?;
        file.appliance.ok_or(SettingsError::NoSection)
    }

    /// The settings as an `[appliance]` section, as `from_toml` reads it.
    pub fn to_toml(&self) -> String {
        format!("[appliance]\nconsole_chaos = {}\n", self.console_chaos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kill_mode_is_off_unless_the_council_says_true() {
        let mut config = HashMap::new();
        assert!(!ApplianceSettings::from_council(&config).console_chaos);
        config.insert(CONSOLE_CHAOS_KEY.to_string(), "yes".to_string());
        assert!(!ApplianceSettings::from_council(&config).console_chaos);
        config.insert(CONSOLE_CHAOS_KEY.to_string(), "true".to_string());
        assert!(ApplianceSettings::from_council(&config).console_chaos);
    }

    #[test]
    fn the_council_entries_read_back_as_the_same_settings() {
        for console_chaos in [false, true] {
            let settings = ApplianceSettings { console_chaos };
            let config: HashMap<String, String> = settings.council_entries().into_iter().collect();
            assert_eq!(ApplianceSettings::from_council(&config), settings);
        }
    }

    #[test]
    fn a_file_is_read_for_its_appliance_section_alone() {
        let text = "[gitops]\nrepository = \"x\"\n\n[appliance]\nconsole_chaos = true\n";
        assert!(ApplianceSettings::from_toml(text).unwrap().console_chaos);
        let empty = ApplianceSettings::from_toml("[appliance]\n").unwrap();
        assert!(!empty.console_chaos, "a missing key is off");
        let settings = ApplianceSettings {
            console_chaos: true,
        };
        assert_eq!(
            ApplianceSettings::from_toml(&settings.to_toml()).unwrap(),
            settings
        );
    }

    #[test]
    fn a_file_without_the_section_or_with_a_misspelt_key_is_refused() {
        assert!(matches!(
            ApplianceSettings::from_toml("[gitops]\n"),
            Err(SettingsError::NoSection)
        ));
        assert!(matches!(
            ApplianceSettings::from_toml("[appliance]\nconsole_choas = true\n"),
            Err(SettingsError::Toml(_))
        ));
    }
}
