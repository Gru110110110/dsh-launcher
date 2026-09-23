use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
};

use fs2::{FileExt, lock_contended_error};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    AppError, AppResult, ApplicationPaths, MigrationPlan,
    import::{
        discover_cc_switch_providers, discover_source_entries, discover_source_workspace,
        import_cc_switch_configuration, import_source_home, import_source_workspace,
    },
    paths::{atomic_write, dirs_home},
};

const COMPLETE: &[u8] = b"1\n";
const MAX_BACKUP_ENTRIES: usize = 100_000;
const MAX_BACKUP_BYTES: u64 = 16 * 1024 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct MigrationService {
    paths: ApplicationPaths,
    source_home: Option<PathBuf>,
    cc_switch_home: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct MigrationOutcome {
    pub plan: MigrationPlan,
    pub warning: Option<AppError>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ManifestKind {
    Directory,
    File { size: u64, digest: [u8; 32] },
    Symlink(OsString),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ManifestEntry {
    relative: PathBuf,
    kind: ManifestKind,
    permissions: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum JournalState {
    Prepared,
    Committed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Journal {
    version: u8,
    transaction_id: Uuid,
    state: JournalState,
}

impl MigrationService {
    pub fn from_environment(paths: ApplicationPaths) -> AppResult<Self> {
        if std::env::var_os("DSH_HOME").is_some() {
            return Ok(Self {
                paths,
                source_home: None,
                cc_switch_home: None,
            });
        }
        let home = dirs_home()?;
        let cc_switch_home = std::env::var_os("DSH_DESKTOP_CC_SWITCH_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| discover_cc_switch_home(&home));
        Ok(Self {
            source_home: Some(
                std::env::var_os("DSH_DESKTOP_SOURCE_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home.join(".dsh")),
            ),
            cc_switch_home: Some(cc_switch_home),
            paths,
        })
    }

    #[cfg(test)]
    fn isolated(paths: ApplicationPaths, source_home: PathBuf, cc_switch_home: PathBuf) -> Self {
        Self {
            paths,
            source_home: Some(source_home),
            cc_switch_home: Some(cc_switch_home),
        }
    }

    pub fn recover(&self) -> AppResult<()> {
        let _lock = self.lock()?;
        self.recover_locked()
    }

    pub fn discover(&self) -> AppResult<Option<MigrationPlan>> {
        if self.source_home.is_none()
            || marker_complete(&self.paths.migration_complete_marker)
            || marker_complete(&self.paths.migration_skip_marker)
            || self.legacy_import_complete()
        {
            return Ok(None);
        }
        let source = self.source_home.as_ref().expect("checked source home");
        let cc_switch = self
            .cc_switch_home
            .as_ref()
            .expect("source homes are configured together");
        let plan = MigrationPlan {
            source_entries: discover_source_entries(source),
            workspace_available: discover_source_workspace(source),
            cc_switch_providers: discover_cc_switch_providers(cc_switch),
        };
        Ok(plan.has_data().then_some(plan))
    }

    pub fn skip(&self) -> AppResult<()> {
        let _lock = self.lock()?;
        self.recover_locked()?;
        if self.source_home.is_none() || marker_complete(&self.paths.migration_complete_marker) {
            return Err(AppError::new("migrationNotAvailable"));
        }
        atomic_write(&self.paths.migration_skip_marker, COMPLETE)?;
        sync_parent(&self.paths.migration_skip_marker)?;
        Ok(())
    }

    pub fn apply(&self) -> AppResult<MigrationOutcome> {
        self.apply_with(import_cc_switch_configuration)
    }

    fn apply_with(
        &self,
        import_cc_switch: impl FnOnce(
            &Path,
            &Path,
            &Path,
        ) -> AppResult<crate::import::CcSwitchImportResult>,
    ) -> AppResult<MigrationOutcome> {
        let _lock = self.lock()?;
        self.recover_locked()?;
        let plan = self
            .discover()?
            .ok_or_else(|| AppError::new("migrationNotAvailable"))?;
        let source = self
            .source_home
            .as_ref()
            .ok_or_else(|| AppError::new("migrationNotAvailable"))?;
        let cc_switch = self
            .cc_switch_home
            .as_ref()
            .ok_or_else(|| AppError::new("migrationNotAvailable"))?;

        ensure_real_directory(&self.paths.dsh_home)?;
        fs::create_dir_all(&self.paths.migration_backups_dir)?;
        owner_only(&self.paths.migration_backups_dir)?;

        let transaction_id = Uuid::new_v4();
        let backup_root = self
            .paths
            .migration_backups_dir
            .join(format!("migration-{transaction_id}"));
        let backup = backup_root.join("dsh-home");
        let rehearsal = backup_root.join("restore-rehearsal");
        let candidate = self.candidate(transaction_id);
        let markers = self.marker_staging(transaction_id);

        let original_manifest = manifest(&self.paths.dsh_home)?;
        fs::create_dir(&backup_root)?;
        owner_only(&backup_root)?;
        if let Err(error) = clone_directory(&self.paths.dsh_home, &backup) {
            remove_owned(&backup_root)?;
            return Err(error);
        }
        let backup_manifest = match manifest(&backup) {
            Ok(manifest) => manifest,
            Err(error) => {
                remove_owned(&backup_root)?;
                return Err(error);
            }
        };
        if backup_manifest != original_manifest {
            remove_owned(&backup_root)?;
            return Err(AppError::new("migrationBackupVerificationFailed"));
        }
        let rehearsal_result = (|| -> AppResult<()> {
            clone_directory(&backup, &rehearsal)?;
            if manifest(&rehearsal)? != original_manifest {
                return Err(AppError::new("migrationRestoreRehearsalFailed"));
            }
            Ok(())
        })();
        if let Err(error) = rehearsal_result {
            remove_owned(&backup_root)?;
            return Err(error);
        }
        remove_owned(&rehearsal)?;

        clone_directory(&backup, &candidate)?;
        fs::create_dir(&markers)?;
        owner_only(&markers)?;
        let applied = (|| -> AppResult<Option<AppError>> {
            import_source_home(source, &candidate, &markers.join("source-home"))?;
            import_source_workspace(source, &candidate, &markers.join("workspace"))?;
            let before_cc_switch = manifest(&candidate)?;
            let readonly_targets = make_cc_switch_targets_writable(&candidate)?;
            let warning = match import_cc_switch(cc_switch, &candidate, &markers.join("cc-switch"))
            {
                Ok(result) => {
                    if !result.imported {
                        restore_readonly(&readonly_targets)?;
                    }
                    None
                }
                Err(error) => {
                    // CC Switch is an optional source. Its importer rolls back
                    // both output files on failure. Verify that rollback before
                    // allowing the candidate to proceed; rebuild only if the
                    // candidate differs so partial data can never be published.
                    restore_readonly(&readonly_targets)?;
                    if manifest(&candidate)? != before_cc_switch {
                        remove_owned(&candidate)?;
                        remove_owned(&markers)?;
                        clone_directory(&backup, &candidate)?;
                        fs::create_dir(&markers)?;
                        owner_only(&markers)?;
                        import_source_home(source, &candidate, &markers.join("source-home"))?;
                        import_source_workspace(source, &candidate, &markers.join("workspace"))?;
                    }
                    let detail = error.safe_detail.unwrap_or(error.code);
                    Some(AppError::new("ccSwitchImportSkipped").detail(detail))
                }
            };
            ensure_real_directory(&candidate)?;
            let _ = manifest(&candidate)?;
            sync_tree(&candidate)?;
            Ok(warning)
        })();
        let warning = match applied {
            Ok(warning) => warning,
            Err(error) => {
                let _ = remove_owned(&candidate);
                let _ = remove_owned(&markers);
                return Err(error);
            }
        };

        self.write_journal(Journal {
            version: 1,
            transaction_id,
            state: JournalState::Prepared,
        })?;
        let previous = self.previous(transaction_id);
        let publication = (|| -> AppResult<()> {
            rename_with_retry(
                &self.paths.dsh_home,
                &previous,
                "migrationPublicationFailed",
            )?;
            sync_parent(&self.paths.dsh_home)?;
            if manifest(&previous)? != original_manifest {
                return Err(AppError::new("configurationChanged"));
            }
            rename_with_retry(
                &candidate,
                &self.paths.dsh_home,
                "migrationPublicationFailed",
            )?;
            sync_parent(&self.paths.dsh_home)?;
            self.write_journal(Journal {
                version: 1,
                transaction_id,
                state: JournalState::Committed,
            })?;
            atomic_write(&self.paths.migration_complete_marker, COMPLETE)?;
            sync_parent(&self.paths.migration_complete_marker)?;
            remove_owned(&previous)?;
            remove_owned(&markers)?;
            fs::remove_file(&self.paths.migration_journal)?;
            sync_parent(&self.paths.migration_journal)
        })();
        if let Err(error) = publication {
            self.recover_locked()?;
            if !marker_complete(&self.paths.migration_complete_marker) {
                return Err(error);
            }
        }
        Ok(MigrationOutcome { plan, warning })
    }

    fn recover_locked(&self) -> AppResult<()> {
        let Some(journal) = read_journal(&self.paths.migration_journal)? else {
            return Ok(());
        };
        if journal.version != 1 {
            return Err(AppError::new("migrationJournalUnsupported"));
        }
        let candidate = self.candidate(journal.transaction_id);
        let previous = self.previous(journal.transaction_id);
        let markers = self.marker_staging(journal.transaction_id);
        let failed = self
            .paths
            .app_home
            .join(format!(".migration-failed-{}", journal.transaction_id));
        match journal.state {
            JournalState::Prepared => {
                if previous.exists() || previous.is_symlink() {
                    remove_owned(&failed)?;
                    if self.paths.dsh_home.exists() || self.paths.dsh_home.is_symlink() {
                        rename_with_retry(
                            &self.paths.dsh_home,
                            &failed,
                            "migrationRecoveryFailed",
                        )?;
                    }
                    rename_with_retry(&previous, &self.paths.dsh_home, "migrationRecoveryFailed")?;
                    remove_owned(&failed)?;
                }
                remove_owned(&candidate)?;
                remove_owned(&markers)?;
                remove_owned(&failed)?;
            }
            JournalState::Committed => {
                if !self.paths.dsh_home.exists() && previous.exists() {
                    rename_with_retry(&previous, &self.paths.dsh_home, "migrationRecoveryFailed")?;
                    let _ = fs::remove_file(&self.paths.migration_complete_marker);
                    remove_owned(&candidate)?;
                    remove_owned(&markers)?;
                    fs::remove_file(&self.paths.migration_journal)?;
                    sync_parent(&self.paths.migration_journal)?;
                    return Ok(());
                }
                ensure_real_directory(&self.paths.dsh_home)?;
                atomic_write(&self.paths.migration_complete_marker, COMPLETE)?;
                remove_owned(&previous)?;
                remove_owned(&candidate)?;
                remove_owned(&markers)?;
            }
        }
        fs::remove_file(&self.paths.migration_journal)?;
        sync_parent(&self.paths.migration_journal)
    }

    fn lock(&self) -> AppResult<File> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&self.paths.migration_lock)
            .map_err(|error| AppError::io_path(&self.paths.migration_lock, &error))?;
        file.try_lock_exclusive().map_err(|error| {
            if error.raw_os_error() == lock_contended_error().raw_os_error() {
                AppError::new("migrationBusy")
            } else {
                AppError::io_path(&self.paths.migration_lock, &error)
            }
        })?;
        Ok(file)
    }

    fn write_journal(&self, journal: Journal) -> AppResult<()> {
        let mut bytes = serde_json::to_vec(&journal)?;
        bytes.push(b'\n');
        atomic_write(&self.paths.migration_journal, &bytes)?;
        sync_parent(&self.paths.migration_journal)
    }

    fn candidate(&self, id: Uuid) -> PathBuf {
        self.paths
            .app_home
            .join(format!(".migration-candidate-{id}"))
    }

    fn previous(&self, id: Uuid) -> PathBuf {
        self.paths
            .app_home
            .join(format!(".migration-previous-{id}"))
    }

    fn marker_staging(&self, id: Uuid) -> PathBuf {
        self.paths.app_home.join(format!(".migration-markers-{id}"))
    }

    fn legacy_import_complete(&self) -> bool {
        [
            &self.paths.home_import_marker,
            &self.paths.workspace_import_marker,
            &self.paths.cc_switch_import_marker,
        ]
        .iter()
        .all(|path| marker_complete(path))
    }
}

fn read_journal(path: &Path) -> AppResult<Option<Journal>> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| AppError::new("migrationJournalInvalid").detail(error.to_string())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(AppError::io_path(path, &error)),
    }
}

fn marker_complete(path: &Path) -> bool {
    fs::read(path).is_ok_and(|bytes| bytes == COMPLETE)
}

fn discover_cc_switch_home(home: &Path) -> PathBuf {
    let default = home.join(".cc-switch");
    #[cfg(windows)]
    {
        if let Some(custom) = read_cc_switch_store_override(home)
            && custom.join("cc-switch.db").is_file()
        {
            return custom;
        }
        if default.join("cc-switch.db").is_file() {
            return default;
        }
        if let Some(legacy_home) = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty() && path != home)
        {
            let legacy = legacy_home.join(".cc-switch");
            if legacy.join("cc-switch.db").is_file() {
                return legacy;
            }
        }
    }
    default
}

#[cfg(windows)]
fn read_cc_switch_store_override(home: &Path) -> Option<PathBuf> {
    const MAX_STORE_BYTES: u64 = 1024 * 1024;

    let store = dirs::data_dir()?
        .join("com.ccswitch.desktop")
        .join("app_paths.json");
    let metadata = fs::symlink_metadata(&store).ok()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > MAX_STORE_BYTES
    {
        return None;
    }
    let document: serde_json::Value = serde_json::from_slice(&fs::read(store).ok()?).ok()?;
    let raw = document.get("app_config_dir_override")?.as_str()?.trim();
    if raw.is_empty() {
        return None;
    }
    if raw == "~" {
        return Some(home.to_owned());
    }
    if let Some(relative) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
        return Some(home.join(relative));
    }
    Some(PathBuf::from(raw))
}

fn rename_with_retry(source: &Path, destination: &Path, code: &'static str) -> AppResult<()> {
    #[cfg(windows)]
    {
        let mut delay = std::time::Duration::from_millis(40);
        for attempt in 0..8 {
            match fs::rename(source, destination) {
                Ok(()) => return Ok(()),
                Err(error)
                    if error.kind() == std::io::ErrorKind::PermissionDenied && attempt < 7 =>
                {
                    std::thread::sleep(delay);
                    delay = delay
                        .saturating_mul(2)
                        .min(std::time::Duration::from_millis(640));
                }
                Err(error) => {
                    return Err(AppError::io(code, &error)
                        .value("source", path_label(source))
                        .value("destination", path_label(destination)));
                }
            }
        }
        unreachable!("the bounded retry loop always returns")
    }
    #[cfg(not(windows))]
    fs::rename(source, destination).map_err(|error| {
        AppError::io(code, &error)
            .value("source", path_label(source))
            .value("destination", path_label(destination))
    })
}

fn path_label(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "application-data".into())
}

#[cfg(windows)]
fn make_cc_switch_targets_writable(dsh_home: &Path) -> AppResult<Vec<PathBuf>> {
    let mut changed = Vec::new();
    for name in ["settings.yaml", ".credentials.yaml"] {
        let path = dsh_home.join(name);
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.permissions().readonly()
        {
            let mut permissions = metadata.permissions();
            permissions.set_readonly(false);
            fs::set_permissions(&path, permissions).map_err(|error| {
                AppError::io("ccSwitchCandidatePermissionFailed", &error).value("path", name)
            })?;
            changed.push(path);
        }
    }
    Ok(changed)
}

#[cfg(not(windows))]
fn make_cc_switch_targets_writable(_dsh_home: &Path) -> AppResult<Vec<PathBuf>> {
    Ok(Vec::new())
}

#[cfg(windows)]
fn restore_readonly(paths: &[PathBuf]) -> AppResult<()> {
    for path in paths {
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_readonly(true);
        fs::set_permissions(path, permissions).map_err(|error| {
            AppError::io("ccSwitchCandidatePermissionFailed", &error)
                .value("path", path_label(path))
        })?;
    }
    Ok(())
}

#[cfg(not(windows))]
fn restore_readonly(_paths: &[PathBuf]) -> AppResult<()> {
    Ok(())
}

fn ensure_real_directory(path: &Path) -> AppResult<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        Ok(())
    } else {
        Err(AppError::new("migrationUnsafeDestination"))
    }
}

fn clone_directory(source: &Path, destination: &Path) -> AppResult<()> {
    ensure_real_directory(source)?;
    fs::create_dir(destination)?;
    owner_only(destination)?;
    let result = clone_children(source, destination);
    if let Err(error) = result {
        let _ = remove_owned(destination);
        return Err(error);
    }
    sync_directory(destination)
}

fn clone_children(source: &Path, destination: &Path) -> AppResult<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            create_symlink(
                &fs::read_link(entry.path())?,
                &target,
                entry.path().is_dir(),
            )?;
        } else if metadata.is_dir() {
            fs::create_dir(&target)?;
            clone_children(&entry.path(), &target)?;
            fs::set_permissions(&target, metadata.permissions())?;
            sync_directory(&target)?;
        } else if metadata.is_file() {
            let mut input = File::open(entry.path())?;
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)?;
            std::io::copy(&mut input, &mut output)?;
            output.sync_all()?;
            fs::set_permissions(&target, metadata.permissions())?;
        } else {
            return Err(AppError::new("migrationUnsupportedEntry"));
        }
    }
    Ok(())
}

fn manifest(root: &Path) -> AppResult<Vec<ManifestEntry>> {
    let mut entries = Vec::new();
    let mut total_bytes = 0u64;
    collect_manifest(root, root, &mut entries, &mut total_bytes)?;
    entries.sort_by(|left, right| left.relative.cmp(&right.relative));
    Ok(entries)
}

fn collect_manifest(
    root: &Path,
    directory: &Path,
    entries: &mut Vec<ManifestEntry>,
    total_bytes: &mut u64,
) -> AppResult<()> {
    for entry in fs::read_dir(directory)? {
        if entries.len() >= MAX_BACKUP_ENTRIES {
            return Err(AppError::new("migrationBackupTooLarge"));
        }
        let entry = entry?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .map_err(|_| AppError::new("migrationUnsafeDestination"))?
            .to_owned();
        let metadata = fs::symlink_metadata(&path)?;
        let kind = if metadata.file_type().is_symlink() {
            ManifestKind::Symlink(fs::read_link(&path)?.into_os_string())
        } else if metadata.is_dir() {
            ManifestKind::Directory
        } else if metadata.is_file() {
            *total_bytes = total_bytes.saturating_add(metadata.len());
            if *total_bytes > MAX_BACKUP_BYTES {
                return Err(AppError::new("migrationBackupTooLarge"));
            }
            let mut file = File::open(&path)?;
            let mut digest = Sha256::new();
            let mut buffer = [0u8; 64 * 1024];
            loop {
                let count = file.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                digest.update(&buffer[..count]);
            }
            ManifestKind::File {
                size: metadata.len(),
                digest: digest.finalize().into(),
            }
        } else {
            return Err(AppError::new("migrationUnsupportedEntry"));
        };
        let recurse = matches!(kind, ManifestKind::Directory);
        entries.push(ManifestEntry {
            relative,
            kind,
            permissions: permission_fingerprint(&metadata),
        });
        if recurse {
            collect_manifest(root, &path, entries, total_bytes)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn permission_fingerprint(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode()
}

#[cfg(not(unix))]
fn permission_fingerprint(metadata: &fs::Metadata) -> u32 {
    u32::from(metadata.permissions().readonly())
}

#[cfg(unix)]
fn create_symlink(target: &Path, link: &Path, _directory: bool) -> AppResult<()> {
    std::os::unix::fs::symlink(target, link)?;
    Ok(())
}

#[cfg(windows)]
fn create_symlink(target: &Path, link: &Path, directory: bool) -> AppResult<()> {
    if directory {
        std::os::windows::fs::symlink_dir(target, link)?;
    } else {
        std::os::windows::fs::symlink_file(target, link)?;
    }
    Ok(())
}

fn remove_owned(path: &Path) -> AppResult<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(path)?
        }
        Ok(_) => fs::remove_file(path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

#[cfg(unix)]
fn owner_only(path: &Path) -> AppResult<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn owner_only(_path: &Path) -> AppResult<()> {
    Ok(())
}

fn sync_parent(path: &Path) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

fn sync_directory(path: &Path) -> AppResult<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn sync_tree(path: &Path) -> AppResult<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            sync_tree(&entry?.path())?;
        }
        sync_directory(path)
    } else if metadata.is_file() {
        // Every file published into the candidate is already synchronized on
        // its writable creation handle. Reopening it with File::open here is
        // read-only; FlushFileBuffers requires GENERIC_WRITE on Windows and
        // otherwise fails deterministically with ERROR_ACCESS_DENIED (5).
        #[cfg(unix)]
        File::open(path)?.sync_all()?;
        #[cfg(not(unix))]
        let _ = path;
        Ok(())
    } else {
        Err(AppError::new("migrationUnsupportedEntry"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inaccessible_migration_lock_identifies_its_path() {
        let temp = tempfile::tempdir().unwrap();
        let paths = ApplicationPaths::from_home(temp.path().join("desktop"));
        paths.ensure_dirs().unwrap();
        fs::create_dir(&paths.migration_lock).unwrap();
        let service = MigrationService::isolated(
            paths.clone(),
            temp.path().join("source"),
            temp.path().join("cc-switch"),
        );

        let error = service.recover().unwrap_err();

        assert_eq!(error.code, "ioPath");
        assert_eq!(
            error.values.get("path"),
            Some(&paths.migration_lock.display().to_string())
        );
        assert!(paths.migration_lock.is_dir());
    }

    #[test]
    fn migration_requires_approval_and_keeps_verified_backup() {
        let temp = tempfile::tempdir().unwrap();
        let paths = ApplicationPaths::from_home(temp.path().join("desktop"));
        paths.ensure_dirs().unwrap();
        fs::write(paths.dsh_home.join("existing"), b"keep").unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("imported"), b"new").unwrap();
        let service = MigrationService::isolated(paths.clone(), source, temp.path().join("cc"));

        assert_eq!(service.discover().unwrap().unwrap().source_entries, 1);
        assert!(!paths.dsh_home.join("imported").exists());
        service.apply().unwrap();

        assert_eq!(fs::read(paths.dsh_home.join("existing")).unwrap(), b"keep");
        assert_eq!(fs::read(paths.dsh_home.join("imported")).unwrap(), b"new");
        assert!(marker_complete(&paths.migration_complete_marker));
        let backup = fs::read_dir(&paths.migration_backups_dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
            .join("dsh-home/existing");
        assert_eq!(fs::read(backup).unwrap(), b"keep");
    }

    #[test]
    fn cc_switch_failure_is_reported_but_does_not_block_source_import() {
        let temp = tempfile::tempdir().unwrap();
        let paths = ApplicationPaths::from_home(temp.path().join("desktop"));
        paths.ensure_dirs().unwrap();
        fs::write(paths.dsh_home.join("existing"), b"keep").unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("imported"), b"new").unwrap();
        let service = MigrationService::isolated(paths.clone(), source, temp.path().join("cc"));

        let outcome = service
            .apply_with(|_, _, _| {
                Err(AppError::new("ccSwitchDatabaseUnreadable").detail("access denied"))
            })
            .unwrap();

        assert_eq!(outcome.plan.source_entries, 1);
        assert_eq!(
            outcome
                .warning
                .as_ref()
                .map(|warning| warning.code.as_str()),
            Some("ccSwitchImportSkipped")
        );
        assert_eq!(fs::read(paths.dsh_home.join("existing")).unwrap(), b"keep");
        assert_eq!(fs::read(paths.dsh_home.join("imported")).unwrap(), b"new");
        assert!(marker_complete(&paths.migration_complete_marker));
    }

    #[test]
    fn rename_errors_include_the_migration_stage_and_relative_targets() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing-candidate");
        let destination = temp.path().join("dsh-home");

        let error =
            rename_with_retry(&missing, &destination, "migrationPublicationFailed").unwrap_err();

        assert_eq!(error.code, "migrationPublicationFailed");
        assert_eq!(
            error.values.get("source").map(String::as_str),
            Some("missing-candidate")
        );
        assert_eq!(
            error.values.get("destination").map(String::as_str),
            Some("dsh-home")
        );
    }

    #[cfg(windows)]
    #[test]
    fn readonly_cc_switch_targets_are_changed_only_in_the_candidate() {
        let temp = tempfile::tempdir().unwrap();
        let candidate = temp.path().join("candidate");
        fs::create_dir(&candidate).unwrap();
        let settings = candidate.join("settings.yaml");
        fs::write(&settings, b"model: keep\n").unwrap();
        let mut permissions = fs::metadata(&settings).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&settings, permissions).unwrap();

        let changed = make_cc_switch_targets_writable(&candidate).unwrap();
        assert_eq!(changed, vec![settings.clone()]);
        assert!(!fs::metadata(&settings).unwrap().permissions().readonly());

        restore_readonly(&changed).unwrap();
        assert!(fs::metadata(&settings).unwrap().permissions().readonly());
    }

    #[cfg(windows)]
    #[test]
    fn windows_migration_publishes_a_candidate_containing_files() {
        let temp = tempfile::tempdir().unwrap();
        let paths = ApplicationPaths::from_home(temp.path().join("desktop"));
        paths.ensure_dirs().unwrap();
        fs::write(paths.dsh_home.join("existing"), b"keep").unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("imported"), b"new").unwrap();
        let service = MigrationService::isolated(paths.clone(), source, temp.path().join("cc"));

        service.apply().unwrap();

        assert_eq!(fs::read(paths.dsh_home.join("existing")).unwrap(), b"keep");
        assert_eq!(fs::read(paths.dsh_home.join("imported")).unwrap(), b"new");
    }

    #[test]
    fn skipping_never_reads_or_copies_source_content() {
        let temp = tempfile::tempdir().unwrap();
        let paths = ApplicationPaths::from_home(temp.path().join("desktop"));
        paths.ensure_dirs().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("secret"), b"not-copied").unwrap();
        let service = MigrationService::isolated(paths.clone(), source, temp.path().join("cc"));
        service.skip().unwrap();
        assert!(service.discover().unwrap().is_none());
        assert!(!paths.dsh_home.join("secret").exists());
    }

    #[test]
    fn prepared_transaction_is_rolled_back_after_interruption() {
        let temp = tempfile::tempdir().unwrap();
        let paths = ApplicationPaths::from_home(temp.path().join("desktop"));
        paths.ensure_dirs().unwrap();
        fs::write(paths.dsh_home.join("value"), b"original").unwrap();
        let service = MigrationService::isolated(
            paths.clone(),
            temp.path().join("source"),
            temp.path().join("cc"),
        );
        let id = Uuid::new_v4();
        let previous = service.previous(id);
        fs::rename(&paths.dsh_home, &previous).unwrap();
        fs::create_dir(&paths.dsh_home).unwrap();
        fs::write(paths.dsh_home.join("value"), b"candidate").unwrap();
        service
            .write_journal(Journal {
                version: 1,
                transaction_id: id,
                state: JournalState::Prepared,
            })
            .unwrap();

        service.recover().unwrap();

        assert_eq!(fs::read(paths.dsh_home.join("value")).unwrap(), b"original");
        assert!(!paths.migration_journal.exists());
    }

    #[test]
    fn committed_transaction_is_completed_after_interruption() {
        let temp = tempfile::tempdir().unwrap();
        let paths = ApplicationPaths::from_home(temp.path().join("desktop"));
        paths.ensure_dirs().unwrap();
        fs::write(paths.dsh_home.join("value"), b"candidate").unwrap();
        let service = MigrationService::isolated(
            paths.clone(),
            temp.path().join("source"),
            temp.path().join("cc"),
        );
        let id = Uuid::new_v4();
        let previous = service.previous(id);
        fs::create_dir(&previous).unwrap();
        fs::write(previous.join("value"), b"original").unwrap();
        service
            .write_journal(Journal {
                version: 1,
                transaction_id: id,
                state: JournalState::Committed,
            })
            .unwrap();

        service.recover().unwrap();

        assert_eq!(
            fs::read(paths.dsh_home.join("value")).unwrap(),
            b"candidate"
        );
        assert!(marker_complete(&paths.migration_complete_marker));
        assert!(!previous.exists());
        assert!(!paths.migration_journal.exists());
    }
}
