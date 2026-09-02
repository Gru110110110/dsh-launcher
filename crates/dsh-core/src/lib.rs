//! Platform-independent application core for DSH Launcher.
//!
//! This crate deliberately has no Tauri dependency. Its services are usable in
//! unit tests with isolated homes and by future UI surfaces without coupling
//! product behavior to a specific window or command handler.

pub mod balance;
pub mod browser;
pub mod child_process;
pub mod error;
pub mod import;
mod log_file;
pub mod marketplace;
pub mod migration;
pub mod model;
pub mod network;
pub mod paths;
pub mod pet;
pub mod preferences;
mod process_recovery;
pub mod remote;
pub mod runtime;
pub mod service;
pub mod startup_repair;
pub mod terminal;

pub use error::{AppError, AppResult};
pub use model::*;
pub use paths::ApplicationPaths;
pub use startup_repair::StartupRepairBackupSummary;
