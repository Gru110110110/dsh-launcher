use std::{fs, path::Path};

use serde::{Deserialize, Serialize};

use crate::{
    AppResult,
    model::{Language, ThemePreference},
    paths::atomic_write,
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
        }
    }
}

impl Preferences {
    pub fn load(path: &Path, legacy_language: &Path) -> Self {
        if let Ok(bytes) = fs::read(path)
            && let Ok(value) = serde_json::from_slice(&bytes)
        {
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
        let mut bytes = serde_json::to_vec_pretty(self)?;
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
}
