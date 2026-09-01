//! Transactional startup repair for versioned, log-derived Harness state.
//!
//! Session logs, credentials, settings, attachments, and workspace ledgers are
//! deliberately outside this module's mutation surface. Only the two official
//! `session_projcache` JSON layouts are moved aside. The old cache remains in a
//! private backup after a verified start and is restored if the retry fails.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use ts_rs::TS;
use walkdir::WalkDir;

use crate::{AppError, AppResult, ApplicationPaths, paths::atomic_write};

const REPAIR_PREFIX: &str = "startup-repair-";
const REPAIR_TRASH_PREFIX: &str = ".startup-repair-trash-";
const RETENTION_MS: u64 = 7 * 24 * 60 * 60 * 1_000;
const MAX_VERIFIED_BACKUPS: usize = 3;
const MAX_BACKUP_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepairManifest {
    status: RepairStatus,
    restore_rehearsed: bool,
    #[serde(default)]
    completed_at_ms: Option<u64>,
    artifacts: Vec<RepairArtifact>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum RepairStatus {
    Preparing,
    Isolated,
    Verified,
    Restored,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepairArtifact {
    source: PathBuf,
    backup: PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct StartupRepairBackupSummary {
    pub count: u32,
    #[ts(type = "number")]
    pub total_bytes: u64,
    #[ts(type = "number | null")]
    pub next_expiry_at_ms: Option<u64>,
    /// In-progress, malformed, or unsafe repair directories are preserved and
    /// reported separately instead of being silently deleted.
    pub protected_count: u32,
}

#[derive(Debug, Clone)]
struct FinalizedBackup {
    path: PathBuf,
    status: RepairStatus,
    completed_at_ms: u64,
    bytes: u64,
}

/// A prepared projection-cache repair. Dropping it never deletes either side;
/// callers explicitly verify or restore after the service retry settles.
#[derive(Debug)]
pub struct ProjectionCacheRepair {
    backup_dir: Option<PathBuf>,
    artifacts: Vec<RepairArtifact>,
}

impl ProjectionCacheRepair {
    /// Move the legacy aggregate cache and current per-record cache aside.
    ///
    /// Every existing artifact is first moved to the backup and restored once
    /// as a recovery rehearsal. Only after that exact restore succeeds is it
    /// moved aside for the real startup attempt.
    pub fn prepare(paths: &ApplicationPaths) -> AppResult<Self> {
        validate_managed_root(&paths.dsh_home)?;
        let storages = paths.dsh_home.join("storages");
        if !storages.exists() {
            return Ok(Self {
                backup_dir: None,
                artifacts: Vec::new(),
            });
        }
        validate_real_directory(&storages)?;

        let targets = [
            storages.join("session_projcache.json"),
            storages.join("session_projcache"),
        ];
        let existing = targets
            .into_iter()
            .filter(|path| path.exists())
            .collect::<Vec<_>>();
        if existing.is_empty() {
            return Ok(Self {
                backup_dir: None,
                artifacts: Vec::new(),
            });
        }
        for target in &existing {
            validate_cache_artifact(target)?;
        }

        fs::create_dir_all(&paths.migration_backups_dir)
            .map_err(|error| AppError::io("startupRepairBackupFailed", &error))?;
        validate_real_directory(&paths.migration_backups_dir)?;
        let backup_dir = unique_backup_dir(&paths.migration_backups_dir);
        fs::create_dir(&backup_dir)
            .map_err(|error| AppError::io("startupRepairBackupFailed", &error))?;

        let artifacts = existing
            .into_iter()
            .map(|source| RepairArtifact {
                backup: backup_dir.join(
                    source
                        .file_name()
                        .expect("fixed projection cache target has a name"),
                ),
                source,
            })
            .collect::<Vec<_>>();
        let mut manifest = RepairManifest {
            status: RepairStatus::Preparing,
            restore_rehearsed: false,
            completed_at_ms: None,
            artifacts: artifacts.clone(),
        };
        write_manifest(&backup_dir, &manifest)?;

        let mut rehearsed = Vec::new();
        for artifact in &artifacts {
            if let Err(error) = rename_artifact(&artifact.source, &artifact.backup) {
                restore_rehearsed(&rehearsed);
                return Err(error);
            }
            if let Err(error) = rename_artifact(&artifact.backup, &artifact.source) {
                // The durable manifest and backup path preserve recovery
                // evidence even if this best-effort immediate restore fails.
                restore_rehearsed(&rehearsed);
                return Err(AppError::new("startupRepairRestoreRehearsalFailed")
                    .value("backup", backup_dir.display())
                    .detail(error.safe_detail.unwrap_or(error.code)));
            }
            rehearsed.push(artifact.clone());
        }
        manifest.restore_rehearsed = true;
        write_manifest(&backup_dir, &manifest)?;

        let mut isolated = Vec::new();
        for artifact in &artifacts {
            if let Err(error) = rename_artifact(&artifact.source, &artifact.backup) {
                restore_isolated(&isolated)?;
                return Err(error);
            }
            isolated.push(artifact.clone());
        }
        manifest.status = RepairStatus::Isolated;
        if let Err(error) = write_manifest(&backup_dir, &manifest) {
            if let Err(restore_error) = restore_isolated(&isolated) {
                return Err(AppError::new("startupRepairRollbackFailed")
                    .value("backup", backup_dir.display())
                    .detail(restore_error.safe_detail.unwrap_or(restore_error.code)));
            }
            return Err(error);
        }

        Ok(Self {
            backup_dir: Some(backup_dir),
            artifacts,
        })
    }

    pub fn changed(&self) -> bool {
        !self.artifacts.is_empty()
    }

    /// Mark a successful service start. The original cache stays in the
    /// timestamped backup instead of being deleted.
    pub fn verify(self) -> AppResult<Option<PathBuf>> {
        let Some(backup_dir) = self.backup_dir else {
            return Ok(None);
        };
        let mut manifest = read_manifest(&backup_dir)?;
        manifest.status = RepairStatus::Verified;
        manifest.completed_at_ms = Some(now_ms());
        write_manifest(&backup_dir, &manifest)?;
        Ok(Some(backup_dir))
    }

    /// Preserve cache files produced by the failed retry, then restore the
    /// exact pre-repair artifacts.
    pub fn restore(self) -> AppResult<()> {
        let Some(backup_dir) = self.backup_dir else {
            return Ok(());
        };
        let rejected_dir = backup_dir.join("failed-retry");
        for artifact in &self.artifacts {
            if artifact.source.exists() {
                fs::create_dir_all(&rejected_dir)
                    .map_err(|error| AppError::io("startupRepairRollbackFailed", &error))?;
                let rejected = rejected_dir.join(
                    artifact
                        .source
                        .file_name()
                        .expect("fixed projection cache target has a name"),
                );
                rename_artifact(&artifact.source, &rejected).map_err(|error| {
                    AppError::new("startupRepairRollbackFailed")
                        .value("backup", backup_dir.display())
                        .detail(error.safe_detail.unwrap_or(error.code))
                })?;
            }
            if artifact.backup.exists() {
                rename_artifact(&artifact.backup, &artifact.source).map_err(|error| {
                    AppError::new("startupRepairRollbackFailed")
                        .value("backup", backup_dir.display())
                        .detail(error.safe_detail.unwrap_or(error.code))
                })?;
            }
        }
        let mut manifest = read_manifest(&backup_dir)?;
        manifest.status = RepairStatus::Restored;
        manifest.completed_at_ms = Some(now_ms());
        write_manifest(&backup_dir, &manifest)
    }
}

/// Describe only finalized, safety-validated startup repair backups. Any
/// unfinished, malformed, or symlink-containing directory is preserved and
/// counted as protected.
pub fn startup_repair_backup_summary(
    paths: &ApplicationPaths,
) -> AppResult<StartupRepairBackupSummary> {
    let (backups, protected_count) = collect_finalized_backups(paths)?;
    Ok(summarize(&backups, protected_count))
}

/// Enforce the bounded retention policy. Callers invoke this only after a
/// managed Harness start has published and verified its service address.
pub fn prune_startup_repair_backups(
    paths: &ApplicationPaths,
) -> AppResult<StartupRepairBackupSummary> {
    let (mut backups, _) = collect_finalized_backups(paths)?;
    backups.sort_by_key(|backup| backup.completed_at_ms);
    let now = now_ms();
    let mut remove = vec![false; backups.len()];

    for (index, backup) in backups.iter().enumerate() {
        if backup.completed_at_ms.saturating_add(RETENTION_MS) <= now {
            remove[index] = true;
        }
    }

    let verified = backups
        .iter()
        .enumerate()
        .filter(|(index, backup)| !remove[*index] && backup.status == RepairStatus::Verified)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    for index in verified
        .iter()
        .take(verified.len().saturating_sub(MAX_VERIFIED_BACKUPS))
    {
        remove[*index] = true;
    }

    let mut retained_bytes = backups
        .iter()
        .enumerate()
        .filter(|(index, _)| !remove[*index])
        .fold(0_u64, |total, (_, backup)| {
            total.saturating_add(backup.bytes)
        });
    if retained_bytes > MAX_BACKUP_BYTES {
        for (index, backup) in backups.iter().enumerate() {
            if retained_bytes <= MAX_BACKUP_BYTES {
                break;
            }
            if remove[index] {
                continue;
            }
            remove[index] = true;
            retained_bytes = retained_bytes.saturating_sub(backup.bytes);
        }
    }

    for (index, backup) in backups.iter().enumerate() {
        if remove[index] {
            remove_backup_dir(&paths.migration_backups_dir, &backup.path)?;
        }
    }
    startup_repair_backup_summary(paths)
}

/// Delete every finalized and safety-validated startup repair backup. Repair
/// transactions that are incomplete, malformed, or unsafe remain untouched.
pub fn clear_startup_repair_backups(
    paths: &ApplicationPaths,
) -> AppResult<StartupRepairBackupSummary> {
    let (backups, _) = collect_finalized_backups(paths)?;
    for backup in backups {
        remove_backup_dir(&paths.migration_backups_dir, &backup.path)?;
    }
    startup_repair_backup_summary(paths)
}

fn validate_managed_root(root: &Path) -> AppResult<()> {
    if !root.exists() {
        return Ok(());
    }
    validate_real_directory(root)
}

fn validate_real_directory(path: &Path) -> AppResult<()> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| AppError::io("startupRepairUnsafePath", &error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(AppError::new("startupRepairUnsafePath").value("path", path.display()));
    }
    Ok(())
}

fn validate_cache_artifact(path: &Path) -> AppResult<()> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| AppError::io("startupRepairUnsafePath", &error))?;
    let expected_directory = path
        .file_name()
        .is_some_and(|name| name == "session_projcache");
    if metadata.file_type().is_symlink()
        || (expected_directory && !metadata.is_dir())
        || (!expected_directory && !metadata.is_file())
    {
        return Err(AppError::new("startupRepairUnsafePath").value("path", path.display()));
    }
    Ok(())
}

fn unique_backup_dir(parent: &Path) -> PathBuf {
    let millis = u128::from(now_ms());
    let base = format!("{REPAIR_PREFIX}{millis}-{}", std::process::id());
    let mut candidate = parent.join(&base);
    let mut suffix = 0_u32;
    while candidate.exists() {
        suffix += 1;
        candidate = parent.join(format!("{base}-{suffix}"));
    }
    candidate
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn collect_finalized_backups(paths: &ApplicationPaths) -> AppResult<(Vec<FinalizedBackup>, u32)> {
    let root = &paths.migration_backups_dir;
    if !root.exists() {
        return Ok((Vec::new(), 0));
    }
    validate_real_directory(root)?;
    let entries = fs::read_dir(root)
        .map_err(|error| AppError::io("startupRepairBackupReadFailed", &error))?;
    let mut backups = Vec::new();
    let mut protected_count = 0_u32;
    for entry in entries {
        let entry = entry.map_err(|error| AppError::io("startupRepairBackupReadFailed", &error))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !valid_repair_name(&name) {
            continue;
        }
        let path = entry.path();
        let Some(bytes) = safe_tree_size(&path) else {
            protected_count = protected_count.saturating_add(1);
            continue;
        };
        let Ok(manifest) = read_manifest(&path) else {
            protected_count = protected_count.saturating_add(1);
            continue;
        };
        if !matches!(
            manifest.status,
            RepairStatus::Verified | RepairStatus::Restored
        ) {
            protected_count = protected_count.saturating_add(1);
            continue;
        }
        let completed_at_ms = manifest.completed_at_ms.or_else(|| {
            fs::metadata(manifest_path(&path))
                .ok()?
                .modified()
                .ok()?
                .duration_since(UNIX_EPOCH)
                .ok()
                .map(|duration| duration.as_millis() as u64)
        });
        let Some(completed_at_ms) = completed_at_ms else {
            protected_count = protected_count.saturating_add(1);
            continue;
        };
        backups.push(FinalizedBackup {
            path,
            status: manifest.status,
            completed_at_ms,
            bytes,
        });
    }
    Ok((backups, protected_count))
}

fn summarize(backups: &[FinalizedBackup], protected_count: u32) -> StartupRepairBackupSummary {
    StartupRepairBackupSummary {
        count: u32::try_from(backups.len()).unwrap_or(u32::MAX),
        total_bytes: backups
            .iter()
            .fold(0_u64, |total, backup| total.saturating_add(backup.bytes)),
        next_expiry_at_ms: backups
            .iter()
            .map(|backup| backup.completed_at_ms.saturating_add(RETENTION_MS))
            .min(),
        protected_count,
    }
}

fn valid_repair_name(name: &str) -> bool {
    name.strip_prefix(REPAIR_PREFIX).is_some_and(|suffix| {
        !suffix.is_empty()
            && suffix
                .chars()
                .all(|character| character.is_ascii_digit() || character == '-')
    })
}

fn safe_tree_size(root: &Path) -> Option<u64> {
    let root_metadata = fs::symlink_metadata(root).ok()?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return None;
    }
    let mut bytes = 0_u64;
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry.ok()?;
        let metadata = fs::symlink_metadata(entry.path()).ok()?;
        if metadata.file_type().is_symlink() {
            return None;
        }
        if metadata.is_file() {
            bytes = bytes.saturating_add(metadata.len());
        } else if !metadata.is_dir() {
            return None;
        }
    }
    Some(bytes)
}

fn remove_backup_dir(root: &Path, target: &Path) -> AppResult<()> {
    if target.parent() != Some(root)
        || !target
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(valid_repair_name)
        || safe_tree_size(target).is_none()
    {
        return Err(AppError::new("startupRepairBackupUnsafe").value("path", target.display()));
    }
    let trash = root.join(format!(
        "{REPAIR_TRASH_PREFIX}{}-{}",
        now_ms(),
        std::process::id()
    ));
    fs::rename(target, &trash)
        .map_err(|error| AppError::io("startupRepairBackupCleanupFailed", &error))?;
    if let Err(error) = fs::remove_dir_all(&trash) {
        let _ = fs::rename(&trash, target);
        return Err(AppError::io("startupRepairBackupCleanupFailed", &error));
    }
    Ok(())
}

fn manifest_path(backup_dir: &Path) -> PathBuf {
    backup_dir.join("manifest.json")
}

fn write_manifest(backup_dir: &Path, manifest: &RepairManifest) -> AppResult<()> {
    let mut bytes = serde_json::to_vec_pretty(manifest)?;
    bytes.push(b'\n');
    atomic_write(&manifest_path(backup_dir), &bytes)
        .map_err(|error| AppError::new("startupRepairBackupFailed").detail(error.to_string()))
}

fn read_manifest(backup_dir: &Path) -> AppResult<RepairManifest> {
    serde_json::from_slice(
        &fs::read(manifest_path(backup_dir))
            .map_err(|error| AppError::io("startupRepairBackupFailed", &error))?,
    )
    .map_err(|error| AppError::new("startupRepairBackupFailed").detail(error.to_string()))
}

fn rename_artifact(source: &Path, destination: &Path) -> AppResult<()> {
    fs::rename(source, destination)
        .map_err(|error| AppError::io("startupRepairBackupFailed", &error))
}

fn restore_rehearsed(artifacts: &[RepairArtifact]) {
    for artifact in artifacts.iter().rev() {
        if !artifact.source.exists() && artifact.backup.exists() {
            let _ = fs::rename(&artifact.backup, &artifact.source);
        }
    }
}

fn restore_isolated(artifacts: &[RepairArtifact]) -> AppResult<()> {
    for artifact in artifacts.iter().rev() {
        if artifact.backup.exists() {
            rename_artifact(&artifact.backup, &artifact.source)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finalized_backup(
        paths: &ApplicationPaths,
        name: &str,
        status: RepairStatus,
        completed_at_ms: u64,
        bytes: u64,
    ) -> PathBuf {
        let backup = paths.migration_backups_dir.join(name);
        fs::create_dir_all(&backup).expect("backup dir");
        let manifest = RepairManifest {
            status,
            restore_rehearsed: true,
            completed_at_ms: Some(completed_at_ms),
            artifacts: Vec::new(),
        };
        write_manifest(&backup, &manifest).expect("manifest");
        let payload = fs::File::create(backup.join("payload.bin")).expect("payload");
        payload.set_len(bytes).expect("sparse payload");
        backup
    }

    fn isolated_paths() -> (tempfile::TempDir, ApplicationPaths) {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = ApplicationPaths::from_home(temp.path().join("desktop"));
        paths.ensure_dirs().expect("isolated desktop dirs");
        fs::create_dir_all(paths.dsh_home.join("storages/session_projcache/sessions"))
            .expect("projection cache tree");
        fs::write(
            paths.dsh_home.join("storages/session_projcache.json"),
            b"legacy",
        )
        .expect("legacy cache");
        fs::write(
            paths
                .dsh_home
                .join("storages/session_projcache/sessions/a.json"),
            b"current",
        )
        .expect("current cache");
        fs::create_dir_all(paths.dsh_home.join("sessions/workspace/session"))
            .expect("session log parent");
        fs::write(
            paths
                .dsh_home
                .join("sessions/workspace/session/session.jsonl.zstd"),
            b"authoritative",
        )
        .expect("session log");
        (temp, paths)
    }

    #[test]
    fn verified_repair_keeps_logs_and_retains_both_cache_layouts_in_backup() {
        let (_temp, paths) = isolated_paths();
        let repair = ProjectionCacheRepair::prepare(&paths).expect("prepare repair");
        assert!(repair.changed());
        assert!(
            !paths
                .dsh_home
                .join("storages/session_projcache.json")
                .exists()
        );
        assert!(!paths.dsh_home.join("storages/session_projcache").exists());
        assert_eq!(
            fs::read(
                paths
                    .dsh_home
                    .join("sessions/workspace/session/session.jsonl.zstd")
            )
            .expect("session remains"),
            b"authoritative"
        );

        let backup = repair.verify().expect("verify repair").expect("backup");
        assert_eq!(
            fs::read(backup.join("session_projcache.json")).unwrap(),
            b"legacy"
        );
        assert_eq!(
            fs::read(backup.join("session_projcache/sessions/a.json")).unwrap(),
            b"current"
        );
    }

    #[test]
    fn failed_repair_preserves_retry_output_and_restores_original_cache() {
        let (_temp, paths) = isolated_paths();
        let repair = ProjectionCacheRepair::prepare(&paths).expect("prepare repair");
        fs::create_dir_all(paths.dsh_home.join("storages/session_projcache/sessions"))
            .expect("retry cache tree");
        fs::write(
            paths
                .dsh_home
                .join("storages/session_projcache/sessions/retry.json"),
            b"retry",
        )
        .expect("retry cache");
        let backup = repair.backup_dir.clone().expect("backup");

        repair.restore().expect("restore repair");

        assert_eq!(
            fs::read(
                paths
                    .dsh_home
                    .join("storages/session_projcache/sessions/a.json")
            )
            .unwrap(),
            b"current"
        );
        assert_eq!(
            fs::read(backup.join("failed-retry/session_projcache/sessions/retry.json")).unwrap(),
            b"retry"
        );
    }

    #[cfg(unix)]
    #[test]
    fn repair_rejects_symlinked_storage_boundaries() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = ApplicationPaths::from_home(temp.path().join("desktop"));
        paths.ensure_dirs().expect("isolated desktop dirs");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&outside).expect("outside");
        std::os::unix::fs::symlink(&outside, paths.dsh_home.join("storages"))
            .expect("storage symlink");

        let error = ProjectionCacheRepair::prepare(&paths).expect_err("reject symlink");
        assert_eq!(error.code, "startupRepairUnsafePath");
    }

    #[test]
    fn retention_expires_old_backups_and_keeps_only_three_verified() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = ApplicationPaths::from_home(temp.path().join("desktop"));
        paths.ensure_dirs().expect("isolated desktop dirs");
        fs::create_dir_all(&paths.migration_backups_dir).expect("backups");
        let now = now_ms();
        finalized_backup(
            &paths,
            "startup-repair-1-1",
            RepairStatus::Restored,
            now.saturating_sub(RETENTION_MS + 1),
            1,
        );
        for index in 0..4_u64 {
            finalized_backup(
                &paths,
                &format!("startup-repair-{}-1", index + 2),
                RepairStatus::Verified,
                now.saturating_sub(4_000 - index * 1_000),
                1,
            );
        }

        let summary = prune_startup_repair_backups(&paths).expect("prune");

        assert_eq!(summary.count, 3);
        assert!(
            !paths
                .migration_backups_dir
                .join("startup-repair-1-1")
                .exists()
        );
        assert!(
            !paths
                .migration_backups_dir
                .join("startup-repair-2-1")
                .exists()
        );
        assert!(
            paths
                .migration_backups_dir
                .join("startup-repair-5-1")
                .is_dir()
        );
    }

    #[test]
    fn retention_enforces_the_total_size_cap_oldest_first() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = ApplicationPaths::from_home(temp.path().join("desktop"));
        paths.ensure_dirs().expect("isolated desktop dirs");
        fs::create_dir_all(&paths.migration_backups_dir).expect("backups");
        let now = now_ms();
        finalized_backup(
            &paths,
            "startup-repair-10-1",
            RepairStatus::Verified,
            now.saturating_sub(2_000),
            300 * 1024 * 1024,
        );
        finalized_backup(
            &paths,
            "startup-repair-11-1",
            RepairStatus::Verified,
            now.saturating_sub(1_000),
            300 * 1024 * 1024,
        );

        let summary = prune_startup_repair_backups(&paths).expect("prune");

        assert_eq!(summary.count, 1);
        assert!(summary.total_bytes <= MAX_BACKUP_BYTES);
        assert!(
            !paths
                .migration_backups_dir
                .join("startup-repair-10-1")
                .exists()
        );
        assert!(
            paths
                .migration_backups_dir
                .join("startup-repair-11-1")
                .is_dir()
        );
    }

    #[cfg(unix)]
    #[test]
    fn manual_cleanup_preserves_symlinked_and_unfinished_backups() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = ApplicationPaths::from_home(temp.path().join("desktop"));
        paths.ensure_dirs().expect("isolated desktop dirs");
        fs::create_dir_all(&paths.migration_backups_dir).expect("backups");
        let safe = finalized_backup(
            &paths,
            "startup-repair-20-1",
            RepairStatus::Verified,
            now_ms(),
            1,
        );
        let unsafe_backup = finalized_backup(
            &paths,
            "startup-repair-21-1",
            RepairStatus::Verified,
            now_ms(),
            1,
        );
        std::os::unix::fs::symlink(temp.path(), unsafe_backup.join("unsafe-link"))
            .expect("unsafe link");
        let unfinished = finalized_backup(
            &paths,
            "startup-repair-22-1",
            RepairStatus::Preparing,
            now_ms(),
            1,
        );
        let migration_backup = paths.migration_backups_dir.join("migration-v1");
        fs::create_dir_all(&migration_backup).expect("unrelated migration backup");
        fs::write(migration_backup.join("user-data"), b"preserve").expect("migration data");

        let summary = clear_startup_repair_backups(&paths).expect("clear safe backups");

        assert!(!safe.exists());
        assert!(unsafe_backup.exists());
        assert!(unfinished.exists());
        assert_eq!(
            fs::read(migration_backup.join("user-data")).expect("migration backup remains"),
            b"preserve"
        );
        assert_eq!(summary.count, 0);
        assert_eq!(summary.protected_count, 2);
    }
}
