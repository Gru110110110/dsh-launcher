use std::{fs, path::Path};

use serde::{Deserialize, Serialize};

use crate::{
    AppResult,
    model::{HarnessUpdateChannel, Language, ProxySettings, ThemePreference},
    paths::atomic_write,
    pet::PetPreferences,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Preferences {
    #[serde(default)]
    pub language: Language,
    #[serde(default)]
    pub theme: ThemePreference,
    #[serde(default = "default_browser")]
    pub browser_id: String,
    /// Whether the Harness dashboard shows the balance card. Defaults to true so
    /// configurations written before this field existed keep the card visible.
    #[serde(default = "default_show_balance_card")]
    pub show_balance_card: bool,
    /// Harness npm dist-tag followed by update checks. Existing preference
    /// files default to the conservative `latest` channel.
    #[serde(default)]
    pub harness_update_channel: HarnessUpdateChannel,
    /// Proxy configuration. Defaults to `system` so configurations written
    /// before proxy support existed keep following the system.
    #[serde(default)]
    pub proxy: ProxySettings,
    /// First-class desktop pet preferences. Existing installations remain
    /// opt-in and therefore do not receive a surprise always-on-top window.
    #[serde(default)]
    pub pet: PetPreferences,
}

fn default_browser() -> String {
    "system".into()
}

fn default_show_balance_card() -> bool {
    true
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            language: system_language(),
            theme: ThemePreference::System,
            browser_id: default_browser(),
            show_balance_card: default_show_balance_card(),
            harness_update_channel: HarnessUpdateChannel::default(),
            proxy: ProxySettings::default(),
            pet: PetPreferences::default(),
        }
    }
}

impl Preferences {
    pub fn load(path: &Path, legacy_language: &Path) -> Self {
        if let Ok(bytes) = fs::read(path)
            && let Ok(mut value) = serde_json::from_slice::<Self>(&bytes)
        {
            // Preferences may be hand-edited or originate from a development
            // build. Never activate or later re-save malformed manual values
            // or hidden inactive fields from disk.
            value.proxy = crate::network::for_persistence(value.proxy).unwrap_or_default();
            return value;
        }
        let mut preferences = Self::default();
        if let Ok(value) = fs::read_to_string(legacy_language) {
            preferences.language = match value.trim() {
                "en" => Language::En,
                "zh" => Language::Zh,
                _ => preferences.language,
            };
        }
        preferences
    }

    pub fn save(&self, path: &Path) -> AppResult<()> {
        // Enforce the invariant here as well as in the command adapter: every
        // caller that persists preferences gets the same credential-safe
        // representation, including unrelated preference updates.
        let mut persisted = self.clone();
        persisted.proxy = crate::network::for_persistence(persisted.proxy)?;
        let mut bytes = serde_json::to_vec_pretty(&persisted)?;
        bytes.push(b'\n');
        atomic_write(path, &bytes)
    }
}

fn system_language() -> Language {
    let locale = sys_locale::get_locale()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if locale.starts_with("zh") {
        Language::Zh
    } else {
        Language::En
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_language_remains_compatible() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("language"), "en\n").unwrap();
        let prefs = Preferences::load(
            &temp.path().join("preferences.json"),
            &temp.path().join("language"),
        );
        assert_eq!(prefs.language, Language::En);
    }

    #[test]
    fn balance_card_defaults_visible_for_existing_preferences() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("preferences.json");
        fs::write(&file, "{\"language\":\"en\",\"theme\":\"dark\"}\n").unwrap();
        let prefs = Preferences::load(&file, &temp.path().join("language"));
        assert!(prefs.show_balance_card);
    }

    #[test]
    fn legacy_preferences_default_to_system_proxy() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("preferences.json");
        fs::write(&file, "{\"language\":\"en\",\"theme\":\"dark\"}\n").unwrap();
        let prefs = Preferences::load(&file, &temp.path().join("language"));
        assert_eq!(prefs.proxy, ProxySettings::default());
        assert_eq!(prefs.proxy.mode, crate::model::ProxyMode::System);
    }

    #[test]
    fn legacy_preferences_default_to_latest_harness_channel() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("preferences.json");
        fs::write(&file, "{\"language\":\"en\",\"theme\":\"dark\"}\n").unwrap();
        let prefs = Preferences::load(&file, &temp.path().join("language"));
        assert_eq!(prefs.harness_update_channel, HarnessUpdateChannel::Latest);
    }

    #[test]
    fn legacy_preferences_keep_the_desktop_pet_opt_in() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("preferences.json");
        fs::write(&file, "{\"language\":\"en\",\"theme\":\"dark\"}\n").unwrap();
        let prefs = Preferences::load(&file, &temp.path().join("language"));
        assert_eq!(prefs.pet, PetPreferences::default());
        assert!(!prefs.pet.enabled);
    }

    #[test]
    fn desktop_pet_preferences_round_trip() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("preferences.json");
        let legacy = temp.path().join("language");
        let prefs = Preferences {
            pet: PetPreferences {
                enabled: true,
                scale: 1.2,
                click_through: true,
                position: Some(crate::pet::PetPosition { x: 80, y: 120 }),
                ..PetPreferences::default()
            },
            ..Preferences::default()
        };
        prefs.save(&file).unwrap();
        assert_eq!(Preferences::load(&file, &legacy).pet, prefs.pet);
    }

    #[test]
    fn harness_update_channel_round_trips() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("preferences.json");
        let legacy = temp.path().join("language");
        let prefs = Preferences {
            harness_update_channel: HarnessUpdateChannel::Alpha,
            ..Preferences::default()
        };
        prefs.save(&file).unwrap();
        assert_eq!(
            Preferences::load(&file, &legacy).harness_update_channel,
            HarnessUpdateChannel::Alpha
        );
    }

    #[test]
    fn proxy_modes_round_trip_through_atomic_save() {
        use crate::model::ProxyMode;
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("preferences.json");
        let legacy = temp.path().join("language");
        for proxy in [
            ProxySettings::default(),
            ProxySettings {
                mode: ProxyMode::Direct,
                ..ProxySettings::default()
            },
            ProxySettings {
                mode: ProxyMode::Manual,
                url: "socks5h://127.0.0.1:1080".into(),
                bypass: "localhost,127.0.0.1".into(),
            },
        ] {
            let mut prefs = Preferences::load(&file, &legacy);
            prefs.proxy = proxy.clone();
            prefs.save(&file).unwrap();
            assert_eq!(Preferences::load(&file, &legacy).proxy, proxy);
        }
    }

    #[test]
    fn invalid_manual_proxy_is_rejected_before_saving() {
        use crate::model::ProxyMode;
        let invalid = ProxySettings {
            mode: ProxyMode::Manual,
            url: "http://user:pass@127.0.0.1:8080".into(),
            bypass: String::new(),
        };
        let error = crate::network::validate(&invalid).expect_err("userinfo must be rejected");
        assert_eq!(error.code, "proxyUrlInvalid");
        assert_eq!(
            error.values.get("reason").map(String::as_str),
            Some("credentials")
        );
        let invalid = ProxySettings {
            mode: ProxyMode::Manual,
            url: "ftp://127.0.0.1:21".into(),
            bypass: String::new(),
        };
        assert_eq!(
            crate::network::validate(&invalid).unwrap_err().code,
            "proxyUrlInvalid"
        );
    }

    #[test]
    fn inactive_proxy_fields_and_credentials_never_persist() {
        use crate::model::ProxyMode;

        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("preferences.json");
        let legacy = temp.path().join("language");
        let prefs = Preferences {
            proxy: ProxySettings {
                mode: ProxyMode::Direct,
                url: "http://user:topsecret@proxy.invalid:8080".into(),
                bypass: "localhost".into(),
            },
            ..Preferences::default()
        };
        prefs.save(&file).unwrap();

        let raw = fs::read_to_string(&file).unwrap();
        assert!(!raw.contains("topsecret"), "{raw}");
        assert!(!raw.contains("user"), "{raw}");
        assert_eq!(
            Preferences::load(&file, &legacy).proxy,
            ProxySettings {
                mode: ProxyMode::Direct,
                url: String::new(),
                bypass: String::new(),
            }
        );
    }

    #[test]
    fn invalid_manual_proxy_from_disk_falls_back_without_rewriting_the_file() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("preferences.json");
        let legacy = temp.path().join("language");
        let raw = r#"{"proxy":{"mode":"manual","url":"http://user:topsecret@proxy.invalid:8080","bypass":""}}"#;
        fs::write(&file, raw).unwrap();

        let prefs = Preferences::load(&file, &legacy);
        assert_eq!(prefs.proxy, ProxySettings::default());
        // Loading is recovery-only and read-only; sanitization happens before
        // activation and the next explicit save replaces the unsafe value.
        assert_eq!(fs::read_to_string(&file).unwrap(), raw);
    }
}
