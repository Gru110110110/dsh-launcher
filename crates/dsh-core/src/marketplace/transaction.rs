use super::*;

/// Durable correlation between one staged profile and its pending-journal
/// update. An unrelated leftover candidate must never trim an older change.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ProfileTransaction {
    pub candidate: String,
    pub backup: String,
    pub source_existed: bool,
    pub previous_pending: Option<Vec<u8>>,
    pub pending_digest: String,
    #[serde(default)]
    pub rolled_back: bool,
}

pub(super) fn sync_directory(path: &Path) -> AppResult<()> {
    #[cfg(unix)]
    fs::File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

impl Marketplace {
    pub(super) fn transaction_file(&self, profile: &str) -> PathBuf {
        self.profile_dir(&format!(".{profile}.market-transaction.json"))
    }

    pub(super) fn recover_recorded_transaction(&self, profile: &str) -> AppResult<bool> {
        let path = self.transaction_file(profile);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        let mut tx: ProfileTransaction = serde_json::from_slice(&bytes).map_err(|e| {
            AppError::new("marketRollbackFailed")
                .detail(format!("invalid profile transaction: {e}"))
        })?;
        let expected_candidate = format!(".{profile}.market-candidate-");
        let expected_backup = format!(".{profile}.market-backup-");
        let safe_name = |name: &str| {
            name.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
        };
        if !tx.candidate.starts_with(&expected_candidate)
            || !safe_name(&tx.candidate)
            || !(tx.backup.starts_with(&expected_backup)
                || tx.backup == format!(".{profile}.market-last-good"))
            || !safe_name(&tx.backup)
        {
            return Err(
                AppError::new("marketRollbackFailed").detail("unsafe profile transaction paths")
            );
        }
        let candidate = self.profile_dir(&tx.candidate);
        let backup = self.profile_dir(&tx.backup);
        let target = self.profile_dir(profile);
        if tx.rolled_back {
            // Recovery already restored the profile and journal. A crash
            // during candidate cleanup must never reclassify this as commit.
            if candidate.exists() {
                fs::remove_dir_all(&candidate)?;
            }
        } else if candidate.exists() {
            // Publication did not finish. Restore the previous active profile
            // before discarding anything; errors retain all recovery material.
            if tx.source_existed && target.exists() && backup.exists() {
                return Err(AppError::new("marketRollbackFailed").detail(
                    "active profile changed during publication; recovery snapshots retained",
                ));
            }
            if tx.source_existed && !target.exists() {
                if !backup.exists() {
                    return Err(
                        AppError::new("marketRollbackFailed").detail("profile backup is missing")
                    );
                }
                fs::rename(&backup, &target)?;
            }
            if !tx.source_existed && backup.exists() {
                if !represents_absent_profile(&backup) && fs::read_dir(&backup)?.next().is_some() {
                    return Err(AppError::new("marketRollbackFailed")
                        .detail("unexpected new-profile backup"));
                }
                fs::remove_dir_all(&backup)?;
            }
            let current = match fs::read(self.pending_file()) {
                Ok(bytes) => Some(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            // If power was lost before the journal write, the earlier
            // journal is already correct and must remain byte-identical.
            if current.as_deref().map(sha256_bytes).as_deref() == Some(&tx.pending_digest) {
                restore_pending_snapshot(&self.pending_file(), tx.previous_pending.as_deref())?;
            } else if current != tx.previous_pending {
                return Err(AppError::new("marketRollbackFailed")
                    .detail("pending journal changed during recovery"));
            }
            if self.catalog_dir().exists() {
                sync_directory(&self.catalog_dir())?;
            }
            sync_directory(&self.profiles_dir())?;
            tx.rolled_back = true;
            crate::paths::atomic_write(&path, &serde_json::to_vec(&tx)?)?;
            sync_directory(&self.profiles_dir())?;
            fs::remove_dir_all(&candidate)?;
        } else {
            // Atomic candidate rename completed. Never roll back a published
            // change merely because its temporary backup still exists.
            if !target.is_dir() {
                return Err(
                    AppError::new("marketRollbackFailed").detail("published profile is missing")
                );
            }
            if tx.backup.starts_with(&expected_backup) && backup.exists() {
                fs::remove_dir_all(&backup)?;
            }
        }
        sync_directory(&self.profiles_dir())?;
        remove_file_if_exists(&path)?;
        Ok(true)
    }
}
