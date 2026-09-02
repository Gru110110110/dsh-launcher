use std::{
    env,
    path::{Path, PathBuf},
};

use crate::{AppError, AppResult};

#[derive(Debug, Clone)]
pub struct ApplicationPaths {
    pub app_home: PathBuf,
    pub runtime_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub node_dir: PathBuf,
    pub node_bin: PathBuf,
    pub dsh_dir: PathBuf,
    pub dsh_bin: PathBuf,
    pub pending_dsh_dir: PathBuf,
    pub pending_harness_update_file: PathBuf,
    pub version_file: PathBuf,
    pub server_log: PathBuf,
    pub install_log: PathBuf,
    pub server_pid: PathBuf,
    pub language_file: PathBuf,
    pub preferences_file: PathBuf,
    pub dsh_home: PathBuf,
    pub terminal_bin_dir: PathBuf,
    pub terminal_dsh_bin: PathBuf,
    pub home_import_marker: PathBuf,
    pub workspace_import_marker: PathBuf,
    pub cc_switch_import_marker: PathBuf,
    pub migration_complete_marker: PathBuf,
    pub migration_skip_marker: PathBuf,
    pub migration_journal: PathBuf,
    pub migration_lock: PathBuf,
    pub migration_backups_dir: PathBuf,
    pub deployment_lock: PathBuf,
    pub launcher_lock: PathBuf,
    /// Balance bridge staging, inside desktop-owned data and never DSH_HOME.
    pub balance_bridge_dir: PathBuf,
    pub balance_bridge_module: PathBuf,
    pub pet_bridge_module: PathBuf,
    pub balance_bridge_overlay: PathBuf,
    pub balance_only_overlay: PathBuf,
    pub balance_bridge_preflight: PathBuf,
    /// Remote-access state, secrets, and the managed cloudflared binary.
    /// Desktop-owned data; never inside DSH_HOME.
    pub remote_dir: PathBuf,
    pub remote_settings_file: PathBuf,
    pub cloudflared_bin: PathBuf,
}

impl ApplicationPaths {
    pub fn from_environment() -> AppResult<Self> {
        let app_home = if let Some(value) = env::var_os("DSH_DESKTOP_HOME") {
            PathBuf::from(value)
        } else {
            dirs_home()?.join(".dsh-desktop")
        };
        Ok(Self::from_home(app_home))
    }

    pub fn from_home(app_home: impl Into<PathBuf>) -> Self {
        let app_home = app_home.into();
        let runtime_dir = app_home.join("runtime");
        let node_dir = runtime_dir.join("node");
        #[cfg(windows)]
        let node_bin = node_dir.join("node.exe");
        #[cfg(not(windows))]
        let node_bin = node_dir.join("bin/node");
        let dsh_dir = runtime_dir.join("dsh");
        let terminal_bin_dir = app_home.join("bin");
        Self {
            cache_dir: app_home.join("cache"),
            node_bin,
            dsh_bin: dsh_dir.join("node_modules/@deepseek-ai/dsh/lib/bin.js"),
            pending_dsh_dir: runtime_dir.join("dsh.pending"),
            pending_harness_update_file: runtime_dir.join("harness-update.pending.json"),
            version_file: runtime_dir.join("runtime.version"),
            server_log: app_home.join("server.log"),
            install_log: app_home.join("install.log"),
            server_pid: app_home.join("server.pid"),
            language_file: app_home.join("language"),
            preferences_file: app_home.join("preferences.json"),
            dsh_home: app_home.join("dsh-home"),
            #[cfg(windows)]
            terminal_dsh_bin: terminal_bin_dir.join("dsh.cmd"),
            #[cfg(not(windows))]
            terminal_dsh_bin: terminal_bin_dir.join("dsh"),
            terminal_bin_dir,
            home_import_marker: app_home.join(".source-home-import-v1"),
            workspace_import_marker: app_home.join(".source-workspace-import-v1"),
            cc_switch_import_marker: app_home.join(".cc-switch-import-v2"),
            migration_complete_marker: app_home.join(".migration-complete-v1"),
            migration_skip_marker: app_home.join(".migration-skip-v1"),
            migration_journal: app_home.join(".migration-journal-v1.json"),
            migration_lock: app_home.join(".migration.lock"),
            migration_backups_dir: app_home.join("backups"),
            deployment_lock: runtime_dir.join(".deployment.lock"),
            launcher_lock: app_home.join(".launcher.lock"),
            balance_bridge_dir: app_home.join("balance").join("bridge"),
            balance_bridge_module: app_home
                .join("balance")
                .join("bridge")
                .join("balance-bridge.mjs"),
            pet_bridge_module: app_home
                .join("balance")
                .join("bridge")
                .join("pet-bridge.mjs"),
            balance_bridge_overlay: app_home
                .join("balance")
                .join("bridge")
                .join("desktop-bridges-overlay.yml"),
            balance_only_overlay: app_home
                .join("balance")
                .join("bridge")
                .join("balance-only-overlay.yml"),
            balance_bridge_preflight: app_home
                .join("balance")
                .join("bridge")
                .join("preflight.json"),
            remote_dir: app_home.join("remote"),
            remote_settings_file: app_home.join("remote").join("settings.json"),
            #[cfg(windows)]
            cloudflared_bin: app_home.join("remote").join("cloudflared.exe"),
            #[cfg(not(windows))]
            cloudflared_bin: app_home.join("remote").join("cloudflared"),
            app_home,
            runtime_dir,
            node_dir,
            dsh_dir,
        }
    }

    pub fn ensure_dirs(&self) -> AppResult<()> {
        for path in [
            &self.app_home,
            &self.runtime_dir,
            &self.cache_dir,
            &self.dsh_home,
        ] {
            std::fs::create_dir_all(path)
                .map_err(|e| AppError::io("createDirectory", &e).value("path", path.display()))?;
        }
        Ok(())
    }
}

pub fn dirs_home() -> AppResult<PathBuf> {
    #[cfg(windows)]
    let home = dirs::home_dir();
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME").map(PathBuf::from);

    home.filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| AppError::new("homeUnavailable"))
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> AppResult<()> {
    use std::io::Write;
    let parent = path.parent().ok_or_else(|| AppError::new("invalidPath"))?;
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(0o600))?;
    }
    temporary.as_file_mut().write_all(bytes)?;
    temporary.as_file_mut().sync_all()?;
    temporary
        .persist(path)
        .map_err(|e| AppError::io("writeFailed", &e.error))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_layout_remains_stable() {
        let paths = ApplicationPaths::from_home("dsh-layout-contract");
        assert!(paths.node_bin.ends_with(if cfg!(windows) {
            "runtime/node/node.exe"
        } else {
            "runtime/node/bin/node"
        }));
        assert!(
            paths
                .dsh_bin
                .ends_with("runtime/dsh/node_modules/@deepseek-ai/dsh/lib/bin.js")
        );
        assert!(paths.dsh_home.ends_with("dsh-home"));
        assert!(paths.terminal_bin_dir.ends_with("bin"));
        assert!(paths.terminal_dsh_bin.ends_with(if cfg!(windows) {
            "bin/dsh.cmd"
        } else {
            "bin/dsh"
        }));
        assert!(paths.launcher_lock.ends_with(".launcher.lock"));
        assert!(
            paths
                .balance_bridge_module
                .ends_with("balance/bridge/balance-bridge.mjs")
        );
        assert!(
            paths
                .pet_bridge_module
                .ends_with("balance/bridge/pet-bridge.mjs")
        );
        assert!(!paths.balance_bridge_dir.starts_with(&paths.dsh_home));
        assert!(!paths.remote_dir.starts_with(&paths.dsh_home));
        assert!(paths.remote_settings_file.ends_with("remote/settings.json"));
        assert!(paths.cloudflared_bin.ends_with(if cfg!(windows) {
            "remote/cloudflared.exe"
        } else {
            "remote/cloudflared"
        }));
        assert!(
            paths
                .cc_switch_import_marker
                .ends_with(".cc-switch-import-v2")
        );
    }
}
