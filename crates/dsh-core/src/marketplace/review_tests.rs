use super::*;

fn change(id: &str, profile: &str, action: MarketOperationKind) -> PendingMarketChange {
    PendingMarketChange {
        plugin_id: id.into(),
        name: id.into(),
        action,
        profile: Some(profile.into()),
    }
}

struct ActivationProbe<'a> {
    market: &'a Marketplace,
    calls: Vec<&'static str>,
    fail_starts: usize,
    fail_stop: bool,
    defer_restart: bool,
}

impl MarketService for ActivationProbe<'_> {
    fn defer_restart(&mut self) -> bool {
        self.defer_restart
    }

    fn stop(&mut self) -> AppResult<()> {
        assert!(self.market.operation_busy());
        self.calls.push("stop");
        if self.fail_stop {
            Err(AppError::new("stopFailed"))
        } else {
            Ok(())
        }
    }

    fn start(&mut self) -> AppResult<()> {
        assert!(
            self.market.begin_operation().is_err(),
            "gate must cover activation"
        );
        self.calls.push("start");
        if self.calls.len() == 2 {
            assert!(
                self.market.last_good_profile("web").exists(),
                "keep rollback until ready"
            );
        }
        if self.fail_starts > 0 {
            self.fail_starts -= 1;
            Err(AppError::new("serviceNoAddress"))
        } else {
            Ok(())
        }
    }
}

fn staged_result(action: MarketOperationKind, profile: &str) -> MarketOperationResult {
    MarketOperationResult {
        ok: true,
        action,
        plugin_id: "owner/group".into(),
        restart_required: true,
        profile: Some(profile.into()),
        error: None,
        setup_steps: Vec::new(),
    }
}

#[cfg(unix)]
#[test]
fn desktop_uninstall_clears_web_and_old_receipt_copies_from_installed_filter() {
    for (defer_restart, fail_starts) in [(false, 0), (true, 0), (false, 1)] {
        let temp = tempfile::tempdir().unwrap();
        let market = super::tests::fake_marketplace(temp.path());
        market.initialize();
        let plugin = super::tests::trading_plugin();
        let packages = install_plan(&plugin)
            .unwrap()
            .packages
            .iter()
            .map(|name| format!("{name}@1.0.0"))
            .collect::<Vec<_>>();
        for profile in ["web", "trading-web"] {
            market
                .mutate_profile_packages(
                    profile,
                    "add",
                    &packages,
                    &change(&plugin.id, profile, MarketOperationKind::Install),
                )
                .unwrap();
            fs::write(
                market.profile_dir(profile).join("cordis.patch.yml"),
                "# preserved\n[]\n",
            )
            .unwrap();
        }
        market
            .mutate_profile_packages(
                "other",
                "add",
                &["alpha@1.0.0".into()],
                &change("owner/other", "other", MarketOperationKind::Install),
            )
            .unwrap();
        market.clear_pending_verification().unwrap();
        let untouched = profile_control_digest(&market.profile_dir("other")).unwrap();
        *market.catalog.lock().unwrap() =
            Some(Arc::new(LoadedCatalog::new(super::tests::catalog(vec![
                plugin.clone(),
            ]))));
        let installed_query = MarketQuery {
            installed: Some(true),
            ..Default::default()
        };
        // Warm the same installed cache used by the marketplace filter.
        let before = market.query(&installed_query).unwrap();
        assert_eq!(before.total, 1);
        let target = before.items[0].installed.as_ref().unwrap();
        assert_eq!(target.profile.as_deref(), Some("web"));
        let _guard = market.begin_operation().unwrap();
        let mut service = ActivationProbe {
            market: &market,
            calls: vec![],
            fail_starts,
            fail_stop: false,
            defer_restart,
        };
        let outcome =
            market.uninstall_desktop_while_guarded(&plugin.id, Some(target), true, &mut service);
        if fail_starts > 0 {
            assert_eq!(outcome.unwrap_err().code, "marketUninstallIncomplete");
            assert_eq!(
                market.query(&installed_query).unwrap().total,
                1,
                "a failed web activation must still show the restored copy"
            );
            assert_eq!(service.calls, ["stop", "start", "stop", "start"]);
        } else {
            let result = outcome.unwrap();
            assert!(result.ok);
            assert_eq!(result.restart_required, defer_restart);
            assert_eq!(market.query(&installed_query).unwrap().total, 0);
            assert!(
                market.query(&MarketQuery::default()).unwrap().items[0]
                    .installed
                    .is_none(),
                "uninstall must not resurrect the old Fix card"
            );
            assert_eq!(
                service.calls,
                if defer_restart {
                    vec![]
                } else {
                    vec!["stop", "start"]
                }
            );
        }
        for profile in ["web", "trading-web"] {
            assert_eq!(
                fs::read_to_string(market.profile_dir(profile).join("cordis.patch.yml")).unwrap(),
                "# preserved\n[]\n"
            );
        }
        assert_eq!(
            profile_control_digest(&market.profile_dir("other")).unwrap(),
            untouched
        );
        assert!(
            read_receipts(&market.profile_dir("trading-web"))
                .unwrap()
                .plugins
                .is_empty()
        );
    }
}

#[cfg(unix)]
#[test]
fn active_work_defers_changes_without_stopping_or_discarding_rollback() {
    for action in [MarketOperationKind::Install, MarketOperationKind::Uninstall] {
        let temp = tempfile::tempdir().unwrap();
        let market = super::tests::fake_marketplace(temp.path());
        let _guard = market.begin_operation().unwrap();
        market
            .mutate_profile_packages(
                "web",
                "add",
                &["alpha@1.0.0".into()],
                &change("owner/group", "web", MarketOperationKind::Install),
            )
            .unwrap();
        let result = if action == MarketOperationKind::Uninstall {
            market
                .clear_web_pending_verification_while_guarded()
                .unwrap();
            let target = market.scan_installed(None).remove(0);
            market
                .uninstall_while_guarded("owner/group", Some(&target), true)
                .unwrap()
        } else {
            staged_result(action, "web")
        };
        let mut service = ActivationProbe {
            market: &market,
            calls: vec![],
            fail_starts: 0,
            fail_stop: false,
            defer_restart: true,
        };
        let pending = market.pending_verification().unwrap();
        let backup = profile_control_digest(&market.last_good_profile("web")).unwrap();
        let result = market
            .activate_operation_while_guarded(result, &mut service)
            .unwrap();
        assert!(result.ok && result.restart_required);
        assert!(
            service.calls.is_empty(),
            "deferred changes must not stop, start or open"
        );
        assert_eq!(market.pending_verification().unwrap(), pending);
        assert_eq!(
            profile_control_digest(&market.last_good_profile("web")).unwrap(),
            backup
        );
        // Explicit activation later still verifies the candidate and commits it.
        service.defer_restart = false;
        let result = market
            .activate_operation_while_guarded(result, &mut service)
            .unwrap();
        assert!(!result.restart_required);
        assert_eq!(service.calls, ["stop", "start"]);
        assert!(market.pending_verification().unwrap().is_none());
    }
}

#[cfg(unix)]
#[test]
fn one_click_web_install_and_uninstall_wait_for_activation_before_commit() {
    let temp = tempfile::tempdir().unwrap();
    let market = super::tests::fake_marketplace(temp.path());
    let _guard = market.begin_operation().unwrap();
    market
        .mutate_profile_packages(
            "web",
            "add",
            &["alpha@1.0.0".into(), "beta@1.0.0".into()],
            &change("owner/group", "web", MarketOperationKind::Install),
        )
        .unwrap();
    let mut service = ActivationProbe {
        market: &market,
        calls: vec![],
        fail_starts: 0,
        fail_stop: false,
        defer_restart: false,
    };
    let result = market
        .activate_operation_while_guarded(
            staged_result(MarketOperationKind::Install, "web"),
            &mut service,
        )
        .unwrap();
    assert!(!result.restart_required);
    assert_eq!(service.calls, ["stop", "start"]);
    assert!(market.pending_verification().unwrap().is_none());
    assert!(!market.last_good_profile("web").exists());
    let target = market.scan_installed(None).remove(0);
    let result = market
        .uninstall_while_guarded("owner/group", Some(&target), true)
        .unwrap();
    service.calls.clear();
    market
        .activate_operation_while_guarded(result, &mut service)
        .unwrap();
    assert_eq!(service.calls, ["stop", "start"]);
    assert!(market.scan_installed(None).is_empty());
    assert!(market.pending_verification().unwrap().is_none());
}

#[cfg(unix)]
#[test]
fn failed_activation_restores_install_and_uninstall_without_another_click() {
    for action in [MarketOperationKind::Install, MarketOperationKind::Uninstall] {
        let temp = tempfile::tempdir().unwrap();
        let market = super::tests::fake_marketplace(temp.path());
        market
            .mutate_profile_packages(
                "web",
                "add",
                &["alpha@1.0.0".into()],
                &change("owner/group", "web", MarketOperationKind::Install),
            )
            .unwrap();
        market.clear_pending_verification().unwrap();
        let active = market.profile_dir("web");
        fs::write(
            active.join("cordis.patch.yml"),
            "# retained user config\n[]\n",
        )
        .unwrap();
        let before = profile_control_digest(&active).unwrap();
        let _guard = market.begin_operation().unwrap();
        let result = if action == MarketOperationKind::Install {
            market
                .mutate_profile_packages(
                    "web",
                    "add",
                    &["beta@1.0.0".into()],
                    &change("owner/second", "web", action),
                )
                .unwrap();
            staged_result(action, "web")
        } else {
            let target = market.scan_installed(None).remove(0);
            market
                .uninstall_while_guarded("owner/group", Some(&target), true)
                .unwrap()
        };
        let mut service = ActivationProbe {
            market: &market,
            calls: vec![],
            fail_starts: 1,
            fail_stop: false,
            defer_restart: false,
        };
        let error = market
            .activate_operation_while_guarded(result, &mut service)
            .unwrap_err();
        assert_eq!(error.code, "marketVerificationFailed");
        assert_eq!(service.calls, ["stop", "start", "stop", "start"]);
        assert_eq!(profile_control_digest(&active).unwrap(), before);
        assert!(market.pending_verification().unwrap().is_none());
    }
}

#[cfg(unix)]
#[test]
fn failed_stop_retains_pending_backup_and_never_starts_or_rolls_back() {
    let temp = tempfile::tempdir().unwrap();
    let market = super::tests::fake_marketplace(temp.path());
    let _guard = market.begin_operation().unwrap();
    market
        .mutate_profile_packages(
            "web",
            "add",
            &["alpha@1.0.0".into()],
            &change("owner/group", "web", MarketOperationKind::Install),
        )
        .unwrap();
    let mut service = ActivationProbe {
        market: &market,
        calls: vec![],
        fail_starts: 0,
        fail_stop: true,
        defer_restart: false,
    };
    assert!(
        market
            .activate_operation_while_guarded(
                staged_result(MarketOperationKind::Install, "web"),
                &mut service
            )
            .is_err()
    );
    assert_eq!(service.calls, ["stop"]);
    assert!(market.pending_verification().unwrap().is_some());
    assert!(market.last_good_profile("web").exists());
}

#[cfg(unix)]
#[test]
fn legacy_uninstall_retains_backup_without_starting_inactive_workloads() {
    let temp = tempfile::tempdir().unwrap();
    let market = super::tests::fake_marketplace(temp.path());
    market
        .mutate_profile_packages(
            "custom",
            "add",
            &["alpha@1.0.0".into()],
            &change("owner/group", "custom", MarketOperationKind::Install),
        )
        .unwrap();
    market.clear_pending_verification().unwrap();
    let before = profile_control_digest(&market.profile_dir("custom")).unwrap();
    let _guard = market.begin_operation().unwrap();
    let target = market.scan_installed(None).remove(0);
    let result = market
        .uninstall_while_guarded("owner/group", Some(&target), false)
        .unwrap();
    let mut service = ActivationProbe {
        market: &market,
        calls: vec![],
        fail_starts: 0,
        fail_stop: false,
        defer_restart: false,
    };
    market
        .activate_operation_while_guarded(result, &mut service)
        .unwrap();
    assert!(service.calls.is_empty());
    assert!(market.pending_verification().unwrap().is_none());
    let backup = fs::read_dir(market.profiles_dir())
        .unwrap()
        .flatten()
        .find(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(".custom.market-retained-")
        })
        .unwrap();
    assert_eq!(profile_control_digest(&backup.path()).unwrap(), before);
}

#[cfg(unix)]
#[test]
fn one_click_uninstall_removes_entire_recorded_group_even_without_catalog() {
    let temp = tempfile::tempdir().unwrap();
    let market = super::tests::fake_marketplace(temp.path());
    let install = change("owner/group", "custom", MarketOperationKind::Install);
    fs::write(
        temp.path().join("package-fixtures.json"),
        r#"{"consumer":{"peerDependencies":{"provider":"^1.0.0"}},"library":{"dsh":{}}}"#,
    )
    .unwrap();
    market
        .mutate_profile_packages(
            "custom",
            "add",
            &[
                "provider@1.0.0".into(),
                "consumer@1.0.0".into(),
                "library@1.0.0".into(),
            ],
            &install,
        )
        .unwrap();
    market.clear_pending_verification().unwrap();
    let active = market.profile_dir("custom");
    fs::write(active.join("cordis.patch.yml"), "# user data\n[]\n").unwrap();
    let entries = market.scan_installed(None);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].packages, ["consumer", "library", "provider"]);
    market
        .uninstall("owner/group", Some(&entries[0]), false)
        .unwrap();
    let manifest = read_manifest(&active.join("package.json")).unwrap();
    assert!(manifest.dependencies.is_empty());
    assert_eq!(manifest.bundles, ["@deepseek-ai/dsh-base"]);
    assert!(read_receipts(&active).unwrap().plugins.is_empty());
    assert_eq!(
        fs::read_to_string(active.join("cordis.patch.yml")).unwrap(),
        "# user data\n[]\n"
    );
    assert!(!active.join("node_modules/consumer").exists());
    assert!(!active.join("node_modules/provider").exists());
    // Staging never discards rollback merely because configuration composes.
    assert!(market.pending_verification().unwrap().is_some());
    assert!(
        read_manifest(&active.join("package.json"))
            .unwrap()
            .dependencies
            .is_empty()
    );
    assert!(market.scan_installed(None).is_empty());
}

#[cfg(unix)]
#[test]
fn automatic_composition_failure_never_publishes_the_candidate() {
    let temp = tempfile::tempdir().unwrap();
    let market = super::tests::fake_marketplace(temp.path());
    fs::write(market.paths.app_home.join("verify-fail"), "fail").unwrap();

    let error = market
        .mutate_profile_packages(
            "custom",
            "add",
            &["alpha@1.0.0".into()],
            &change("owner/alpha", "custom", MarketOperationKind::Install),
        )
        .unwrap_err();

    assert_eq!(error.code, "marketProfileVerificationFailed");
    assert!(!market.profile_dir("custom").exists());
    assert!(market.pending_verification().unwrap().is_none());
}

#[cfg(unix)]
#[test]
fn shared_and_preexisting_packages_survive_and_orphaned_market_packages_are_cleaned_later() {
    let temp = tempfile::tempdir().unwrap();
    let market = super::tests::fake_marketplace(temp.path());
    fs::create_dir_all(market.profiles_dir()).unwrap();
    let active = market.profile_dir("custom");
    create_profile_candidate(&active, "custom", &new_profile_manifest("custom")).unwrap();
    let mut manifest = read_manifest(&active.join("package.json")).unwrap();
    manifest
        .dependencies
        .insert("preexisting".into(), "1.0.0".into());
    manifest.bundles.push("preexisting".into());
    write_manifest(&active.join("package.json"), &manifest).unwrap();
    fs::create_dir_all(active.join("node_modules/preexisting")).unwrap();
    fs::write(
        active.join("node_modules/preexisting/package.json"),
        r#"{"name":"preexisting","version":"1.0.0","dsh":{"bundle":{"patch":{}}}}"#,
    )
    .unwrap();
    for (id, package) in [("owner/one", "one"), ("owner/two", "two")] {
        let install = change(id, "custom", MarketOperationKind::Install);
        market
            .mutate_profile_packages(
                "custom",
                "add",
                &[
                    "shared@1.0.0".into(),
                    "preexisting@1.0.0".into(),
                    format!("{package}@1.0.0"),
                ],
                &install,
            )
            .unwrap();
    }
    let one = market
        .scan_installed(None)
        .into_iter()
        .find(|e| e.plugin_id.as_deref() == Some("owner/one"))
        .unwrap();
    assert_eq!(one.packages, ["one"]);
    assert!(one.retained_packages.contains(&"shared".into()));
    assert!(one.retained_packages.contains(&"preexisting".into()));
    market.uninstall("owner/one", Some(&one), false).unwrap();
    let two = market
        .scan_installed(None)
        .into_iter()
        .find(|e| e.plugin_id.as_deref() == Some("owner/two"))
        .unwrap();
    assert_eq!(two.packages, ["shared", "two"]);
    market.uninstall("owner/two", Some(&two), false).unwrap();
    let manifest = read_manifest(&active.join("package.json")).unwrap();
    assert_eq!(
        manifest.dependencies.keys().cloned().collect::<Vec<_>>(),
        ["preexisting"]
    );
    assert!(read_receipts(&active).unwrap().managed_packages.is_empty());
}

#[cfg(unix)]
#[test]
fn uninstall_failure_and_concurrent_patch_edit_never_publish_partial_changes() {
    let temp = tempfile::tempdir().unwrap();
    let market = super::tests::fake_marketplace(temp.path());
    let install = change("owner/group", "custom", MarketOperationKind::Install);
    let specs = vec!["alpha@1.0.0".into(), "beta@1.0.0".into()];
    market
        .mutate_profile_packages("custom", "add", &specs, &install)
        .unwrap();
    let active = market.profile_dir("custom");
    let digest = profile_control_digest(&active).unwrap();
    let pending = fs::read(market.pending_file()).unwrap();
    fs::write(temp.path().join("fail"), "").unwrap();
    let target = market.scan_installed(None).remove(0);
    assert!(
        market
            .uninstall("owner/group", Some(&target), false)
            .is_err()
    );
    assert_eq!(profile_control_digest(&active).unwrap(), digest);
    assert_eq!(fs::read(market.pending_file()).unwrap(), pending);
    fs::remove_file(temp.path().join("fail")).unwrap();
    fs::write(temp.path().join("edit-profile"), "custom").unwrap();
    let error = market
        .mutate_profile_packages(
            "custom",
            "add",
            &["gamma@1.0.0".into()],
            &change("owner/other", "custom", MarketOperationKind::Install),
        )
        .unwrap_err();
    assert_eq!(error.code, "marketProfileChanged");
    assert!(
        !read_manifest(&active.join("package.json"))
            .unwrap()
            .dependencies
            .contains_key("gamma")
    );
    assert!(
        fs::read_to_string(active.join("cordis.patch.yml"))
            .unwrap()
            .contains("edited concurrently")
    );
    assert_eq!(fs::read(market.pending_file()).unwrap(), pending);
}

#[test]
fn unrelated_candidate_cannot_erase_previous_pending_change() {
    let temp = tempfile::tempdir().unwrap();
    let market = Marketplace::new(ApplicationPaths::from_home(temp.path().join("home")));
    fs::create_dir_all(market.profile_dir("web")).unwrap();
    fs::create_dir_all(market.profile_dir(".web.market-candidate-unrelated")).unwrap();
    market
        .write_pending_change("owner/old", "old", MarketOperationKind::Install, "web")
        .unwrap();
    let before = fs::read(market.pending_file()).unwrap();
    market.recover_profile_transaction("web").unwrap();
    assert_eq!(fs::read(market.pending_file()).unwrap(), before);
}

#[cfg(unix)]
#[test]
fn web_start_commit_or_rollback_does_not_touch_custom_profiles() {
    for commit in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let market = super::tests::fake_marketplace(temp.path());
        for profile in ["web", "custom"] {
            market
                .mutate_profile_packages(
                    profile,
                    "add",
                    &["alpha@1.0.0".into()],
                    &change("owner/group", profile, MarketOperationKind::Install),
                )
                .unwrap();
        }
        assert_eq!(market.pending_web_change_summary(), "owner/group");
        let custom = profile_control_digest(&market.profile_dir("custom")).unwrap();
        if commit {
            market
                .clear_web_pending_verification_while_guarded()
                .unwrap();
        } else {
            market.rollback_web_pending_while_guarded().unwrap();
        }
        assert_eq!(
            profile_control_digest(&market.profile_dir("custom")).unwrap(),
            custom
        );
        assert!(market.last_good_profile("custom").exists());
        let pending = market.pending_verification().unwrap().unwrap();
        assert_eq!(pending.changes.len(), 1);
        assert_eq!(pending.changes[0].profile.as_deref(), Some("custom"));
        assert!(!market.has_pending_web_rollback());
        assert!(market.pending_web_change_summary().is_empty());
    }
}

#[test]
fn transaction_recovers_each_publication_boundary_and_preserves_earlier_journal() {
    for phase in 0..4 {
        let temp = tempfile::tempdir().unwrap();
        let market = Marketplace::new(ApplicationPaths::from_home(temp.path().join("home")));
        let active = market.profile_dir("web");
        let candidate = market.profile_dir(".web.market-candidate-test");
        let backup = market.profile_dir(".web.market-backup-test");
        fs::create_dir_all(&active).unwrap();
        fs::create_dir_all(&candidate).unwrap();
        fs::write(active.join("package.json"), "old").unwrap();
        fs::write(candidate.join("package.json"), "new").unwrap();
        market
            .write_pending_change("owner/old", "old", MarketOperationKind::Install, "web")
            .unwrap();
        let before = fs::read(market.pending_file()).unwrap();
        let after = market
            .pending_change_bytes("owner/new", "new", MarketOperationKind::Install, "web")
            .unwrap();
        let tx = ProfileTransaction {
            rolled_back: false,
            candidate: ".web.market-candidate-test".into(),
            backup: ".web.market-backup-test".into(),
            source_existed: true,
            previous_pending: Some(before.clone()),
            pending_digest: sha256_bytes(&after),
        };
        crate::paths::atomic_write(
            &market.transaction_file("web"),
            &serde_json::to_vec(&tx).unwrap(),
        )
        .unwrap();
        if phase >= 1 {
            fs::write(market.pending_file(), &after).unwrap();
        }
        if phase >= 2 {
            fs::rename(&active, &backup).unwrap();
        }
        if phase >= 3 {
            fs::rename(&candidate, &active).unwrap();
        }
        market.recover_profile_transaction("web").unwrap();
        assert_eq!(
            fs::read_to_string(active.join("package.json")).unwrap(),
            if phase == 3 { "new" } else { "old" }
        );
        assert_eq!(
            fs::read(market.pending_file()).unwrap(),
            if phase == 3 { after } else { before }
        );
        assert!(!market.transaction_file("web").exists());
        assert!(!candidate.exists());
        assert!(!backup.exists());
    }
}

#[cfg(unix)]
#[test]
fn force_policy_reaches_pnpm_without_enabling_lifecycle_scripts() {
    let temp = tempfile::tempdir().unwrap();
    let market = super::tests::fake_marketplace(temp.path());
    market
        .mutate_profile_packages_with_policy(
            "custom",
            "add",
            &["alpha@1.0.0".into()],
            &change("owner/group", "custom", MarketOperationKind::Install),
            true,
        )
        .unwrap();
    let calls = fs::read_to_string(temp.path().join("calls.jsonl")).unwrap();
    assert!(calls.contains("--no-strict-peer-dependencies"));
    assert!(calls.contains("--ignore-scripts"));
}

#[cfg(unix)]
#[test]
fn composition_check_alone_never_discards_rollback() {
    let temp = tempfile::tempdir().unwrap();
    let market = super::tests::fake_marketplace(temp.path());
    for profile in ["custom", "web"] {
        market
            .mutate_profile_packages(
                profile,
                "add",
                &["alpha@1.0.0".into()],
                &change("owner/one", profile, MarketOperationKind::Install),
            )
            .unwrap();
        let pending = market.pending_verification().unwrap();
        market.verify_profile_composition(profile).unwrap();
        assert_eq!(market.pending_verification().unwrap(), pending);
        assert!(market.last_good_profile(profile).exists());
    }
}

/// Run via scripts/test-marketplace-real-pnpm.py, which supplies a loopback
/// fixture registry and a pnpm shim with physically isolated stores/config.
#[cfg(unix)]
#[test]
#[ignore = "requires isolated real pnpm fixture registry"]
fn real_pnpm_group_lifecycle() {
    let bin = PathBuf::from(
        std::env::var_os("DSH_MARKET_REAL_PNPM_BIN_DIR").expect("use fixture script"),
    );
    assert!(bin.starts_with(std::env::temp_dir()));
    let temp = tempfile::tempdir().unwrap();
    let market = super::tests::fake_marketplace(temp.path());
    *market.pnpm_bin.lock().unwrap() = Some(bin);
    let specs: Vec<_> = ["provider", "consumer", "library"]
        .iter()
        .map(|name| format!("dsh-market-fixture-{name}@1.0.0"))
        .collect();
    market
        .mutate_profile_packages(
            "custom",
            "add",
            &specs,
            &change("fixture/group", "custom", MarketOperationKind::Install),
        )
        .unwrap();
    let active = market.profile_dir("custom");
    market.clear_pending_verification().unwrap();
    fs::write(
        active.join("cordis.patch.yml"),
        "# user configuration\n[]\n",
    )
    .unwrap();
    let before = profile_control_digest(&active).unwrap();
    for name in ["missing", "plain-library"] {
        assert!(
            market
                .mutate_profile_packages(
                    "custom",
                    "add",
                    &[format!("dsh-market-fixture-{name}@1.0.0")],
                    &change("fixture/failure", "custom", MarketOperationKind::Install)
                )
                .is_err()
        );
        assert_eq!(profile_control_digest(&active).unwrap(), before);
        assert!(market.pending_verification().unwrap().is_none());
    }
    market
        .mutate_profile_packages_with_policy(
            "custom",
            "add",
            &["dsh-market-fixture-bad-peer@1.0.0".into()],
            &change("fixture/forced", "custom", MarketOperationKind::Install),
            true,
        )
        .unwrap();
    market.rollback_pending().unwrap();
    assert_eq!(profile_control_digest(&active).unwrap(), before);
    let target = market
        .scan_installed(None)
        .into_iter()
        .find(|p| p.plugin_id.as_deref() == Some("fixture/group"))
        .unwrap();
    assert_eq!(target.packages.len(), 3);
    market
        .uninstall("fixture/group", Some(&target), false)
        .unwrap();
    assert!(
        read_manifest(&active.join("package.json"))
            .unwrap()
            .dependencies
            .is_empty()
    );
    for name in ["provider", "consumer", "library"] {
        assert!(
            !active
                .join(format!("node_modules/dsh-market-fixture-{name}"))
                .exists()
        );
    }
    assert_eq!(
        fs::read_to_string(active.join("cordis.patch.yml")).unwrap(),
        "# user configuration\n[]\n"
    );
    market.rollback_pending().unwrap();
    assert_eq!(profile_control_digest(&active).unwrap(), before);
    assert_eq!(market.scan_installed(None).len(), 1);
}

#[test]
fn new_profile_interruption_before_absence_marker_restores_previous_journal() {
    let temp = tempfile::tempdir().unwrap();
    let market = Marketplace::new(ApplicationPaths::from_home(temp.path().join("home")));
    let candidate = market.profile_dir(".custom.market-candidate-test");
    let backup = market.last_good_profile("custom");
    fs::create_dir_all(&candidate).unwrap();
    fs::create_dir(&backup).unwrap();
    market
        .write_pending_change("owner/new", "new", MarketOperationKind::Install, "custom")
        .unwrap();
    let tx = ProfileTransaction {
        rolled_back: false,
        candidate: ".custom.market-candidate-test".into(),
        backup: ".custom.market-last-good".into(),
        source_existed: false,
        previous_pending: None,
        pending_digest: sha256_bytes(&fs::read(market.pending_file()).unwrap()),
    };
    fs::write(
        market.transaction_file("custom"),
        serde_json::to_vec(&tx).unwrap(),
    )
    .unwrap();
    market.recover_profile_transaction("custom").unwrap();
    assert!(!market.profile_dir("custom").exists());
    assert!(!backup.exists());
    assert!(!candidate.exists());
    assert!(market.pending_verification().unwrap().is_none());
}

#[cfg(unix)]
#[test]
fn stale_group_uninstall_cannot_remove_newly_shared_packages() {
    let temp = tempfile::tempdir().unwrap();
    let market = super::tests::fake_marketplace(temp.path());
    market
        .mutate_profile_packages(
            "custom",
            "add",
            &["alpha@1.0.0".into()],
            &change("owner/one", "custom", MarketOperationKind::Install),
        )
        .unwrap();
    let stale = market.scan_installed(None).remove(0);
    market
        .mutate_profile_packages(
            "custom",
            "add",
            &["alpha@1.0.0".into()],
            &change("owner/two", "custom", MarketOperationKind::Install),
        )
        .unwrap();
    assert!(market.uninstall("owner/one", Some(&stale), false).is_err());
    let one = market
        .scan_installed(None)
        .into_iter()
        .find(|p| p.plugin_id.as_deref() == Some("owner/one"))
        .unwrap();
    assert!(one.packages.is_empty());
    market.uninstall("owner/one", Some(&one), false).unwrap();
    assert!(
        market
            .profile_dir("custom")
            .join("node_modules/alpha")
            .is_dir()
    );
    let two = market
        .scan_installed(None)
        .into_iter()
        .find(|p| p.plugin_id.as_deref() == Some("owner/two"))
        .unwrap();
    assert_eq!(two.packages, ["alpha"]);
}

#[test]
fn interrupted_rollback_cleanup_is_idempotent_for_an_absent_new_profile() {
    for candidate_left in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let market = Marketplace::new(ApplicationPaths::from_home(temp.path().join("home")));
        let candidate = market.profile_dir(".custom.market-candidate-test");
        fs::create_dir_all(market.profiles_dir()).unwrap();
        if candidate_left {
            fs::create_dir(&candidate).unwrap();
        }
        market
            .write_pending_change(
                "owner/earlier",
                "earlier",
                MarketOperationKind::Install,
                "web",
            )
            .unwrap();
        let previous = fs::read(market.pending_file()).unwrap();
        // Profile and journal restoration finished; interruption occurred
        // immediately before or after removing the candidate directory.
        let tx = ProfileTransaction {
            candidate: ".custom.market-candidate-test".into(),
            backup: ".custom.market-last-good".into(),
            source_existed: false,
            previous_pending: Some(previous.clone()),
            pending_digest: "superseded".into(),
            rolled_back: true,
        };
        fs::write(
            market.transaction_file("custom"),
            serde_json::to_vec(&tx).unwrap(),
        )
        .unwrap();
        market.recover_all_profile_transactions().unwrap();
        market.recover_all_profile_transactions().unwrap();
        assert!(!market.profile_dir("custom").exists());
        assert!(!candidate.exists());
        assert!(!market.transaction_file("custom").exists());
        assert_eq!(fs::read(market.pending_file()).unwrap(), previous);
    }
}
