use super::*;

/// OS/UI adapters own the process. The marketplace owns the transaction and
/// keeps its mutation gate held until the changed service is usable or restored.
pub trait MarketService {
    /// Preserve active work, unknown activity, and an earlier deferred batch.
    fn defer_restart(&mut self) -> bool;
    fn stop(&mut self) -> AppResult<()>;
    fn start(&mut self) -> AppResult<()>;
}

impl Marketplace {
    /// A desktop card represents a marketplace plugin, including receipt-backed
    /// copies left by older launchers. Remove those copies as part of the same
    /// user action; keep single-location semantics for unowned legacy packages.
    pub fn uninstall_desktop_while_guarded(
        &self,
        plugin_id: &str,
        target: Option<&InstalledPlugin>,
        service_running: bool,
        service: &mut (impl MarketService + ?Sized),
    ) -> AppResult<MarketOperationResult> {
        self.initialize();
        let target = target.ok_or_else(|| AppError::new("marketUninstallTargetRequired"))?;
        let catalog = self.catalog.lock().expect("catalog poisoned").clone();
        let installed = self.scan_installed(catalog.as_ref().map(|catalog| &catalog.file));
        // Validate the card before changing any other profile.
        if !installed.iter().any(|entry| {
            (entry.plugin_id.as_deref() == Some(plugin_id)
                || entry.local_name.eq_ignore_ascii_case(plugin_id))
                && same_install_location(entry, target)
        }) {
            return Err(AppError::new("marketNotInstalled").value("plugin", plugin_id));
        }
        let mut targets: Vec<_> = installed
            .into_iter()
            .filter(|entry| {
                ((entry.plugin_id.as_deref() == Some(plugin_id)
                    || entry.local_name.eq_ignore_ascii_case(plugin_id))
                    && same_install_location(entry, target))
                    || (target.source == PluginSource::Profile
                        && entry.source == PluginSource::Profile
                        && entry.grouped
                        && entry.plugin_id.as_deref() == Some(plugin_id))
            })
            .collect();
        // Inactive copies need no service restart. Process web last, so the
        // existing Harness startup opens the browser once after all removals.
        targets.sort_by_key(|entry| {
            (
                entry.profile.as_deref() == Some(DEFAULT_PROFILE),
                entry.profile.clone(),
            )
        });
        let mut last_result = None;
        for entry in targets {
            let outcome = self
                .uninstall_while_guarded(plugin_id, Some(&entry), service_running)
                .and_then(|result| self.activate_operation_while_guarded(result, service));
            match outcome {
                Ok(result) => last_result = Some(result),
                Err(error) if last_result.is_some() => {
                    // Each profile retains its own recovery transaction. Never
                    // report complete success when a later copy failed removal.
                    return Err(AppError::new("marketUninstallIncomplete")
                        .value("plugin", plugin_id)
                        .value("profile", entry.profile.as_deref().unwrap_or("skills"))
                        .detail(error.safe_detail.unwrap_or(error.code)));
                }
                Err(error) => return Err(error),
            }
        }
        last_result.ok_or_else(|| AppError::new("marketNotInstalled").value("plugin", plugin_id))
    }

    pub fn activate_operation_while_guarded(
        &self,
        mut result: MarketOperationResult,
        service: &mut (impl MarketService + ?Sized),
    ) -> AppResult<MarketOperationResult> {
        let Some(profile) = result.profile.as_deref() else {
            return Ok(result);
        };
        if profile != DEFAULT_PROFILE {
            // Only old installations can reach this path: new desktop installs
            // always target web. Uninstalling an inactive legacy copy does not
            // boot its potentially headless workloads. Retain its backup instead
            // of claiming that dump-config proved it was safe to discard.
            if result.action != MarketOperationKind::Uninstall {
                return Err(
                    AppError::new("marketProfileVerificationFailed").value("profile", profile)
                );
            }
            self.recover_profile_transaction(profile)?;
            let backup = self.last_good_profile(profile);
            if backup.exists() {
                fs::rename(
                    &backup,
                    self.profile_dir(&format!(
                        ".{profile}.market-retained-{}",
                        process_timestamp()
                    )),
                )?;
                sync_directory(&self.profiles_dir())?;
            }
            self.remove_pending_profile(profile)?;
            result.restart_required = false;
            return Ok(result);
        }

        if service.defer_restart() {
            result.restart_required = true;
            return Ok(result);
        }

        // If stopping fails, keep the pending journal and both snapshots. Never
        // rename a rollback over a process whose shutdown we cannot establish.
        service.stop()?;
        if let Err(start_error) = service.start() {
            service.stop()?;
            self.rollback_web_pending_while_guarded()?;
            service.start().map_err(|restore_error| {
                AppError::new("marketRestoreFailed")
                    .detail(restore_error.safe_detail.unwrap_or(restore_error.code))
            })?;
            return Err(AppError::new("marketVerificationFailed")
                .value("plugins", &result.plugin_id)
                .detail(start_error.safe_detail.unwrap_or(start_error.code)));
        }
        if let Err(error) = self.clear_web_pending_verification_while_guarded() {
            // A healthy start is the commit boundary; cleanup failure must not
            // misreport a successful installation as failed. The durable verified
            // directory/journal retries cleanup before the next mutation.
            log::warn!("verified marketplace cleanup pending: {error}");
        }
        result.restart_required = false;
        // Harness already opens its launch URL during startup. Do not launch
        // the browser again from the marketplace activation path.
        Ok(result)
    }
}
