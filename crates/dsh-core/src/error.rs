use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use ts_rs::TS;

pub type AppResult<T> = Result<T, AppError>;

/// Stable, localizable error contract. Internal error chains are logged by the
/// adapter and never exposed as UI translation keys.
#[derive(Debug, Clone, Error, PartialEq, Eq, Serialize, Deserialize, TS)]
#[error("{code}: {safe_detail:?}")]
#[serde(rename_all = "camelCase")]
#[ts(rename = "LauncherError")]
pub struct AppError {
    pub code: String,
    #[serde(default)]
    pub values: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub safe_detail: Option<String>,
}

impl AppError {
    pub fn new(code: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            values: BTreeMap::new(),
            safe_detail: None,
        }
    }

    pub fn value(mut self, key: impl Into<String>, value: impl ToString) -> Self {
        self.values.insert(key.into(), value.to_string());
        self
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.safe_detail = Some(detail.into());
        self
    }

    pub fn io(code: &'static str, error: &std::io::Error) -> Self {
        Self::new(code).detail(error.to_string())
    }

    pub fn io_path(path: &Path, error: &std::io::Error) -> Self {
        Self::io("ioPath", error).value("path", path.display())
    }
}

impl From<std::io::Error> for AppError {
    fn from(value: std::io::Error) -> Self {
        Self::io("io", &value)
    }
}

impl From<serde_json::Error> for AppError {
    fn from(value: serde_json::Error) -> Self {
        Self::new("invalidData").detail(value.to_string())
    }
}
