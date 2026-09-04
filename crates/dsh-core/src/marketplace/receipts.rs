use super::*;

const RECEIPTS_FILE: &str = ".dsh-market-installations.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct InstallReceipts {
    pub schema_version: u32,
    pub managed_packages: std::collections::BTreeSet<String>,
    pub plugins: BTreeMap<String, InstallReceipt>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct InstallReceipt {
    pub name: String,
    pub packages: Vec<String>,
}

pub(super) fn read_receipts(profile: &Path) -> AppResult<InstallReceipts> {
    let bytes = match fs::read(profile.join(RECEIPTS_FILE)) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(InstallReceipts {
                schema_version: 1,
                ..Default::default()
            });
        }
        Err(e) => return Err(e.into()),
    };
    let receipts: InstallReceipts = serde_json::from_slice(&bytes).map_err(|e| {
        AppError::new("marketProfileInvalid")
            .detail(format!("invalid marketplace installation record: {e}"))
    })?;
    let valid_package = |p: &String| normalize_package_spec(p).as_deref() == Some(p.as_str());
    if receipts.schema_version != 1
        || !receipts.managed_packages.iter().all(valid_package)
        || receipts.plugins.iter().any(|(id, r)| {
            !valid_github_repo_id(id)
                || r.packages.is_empty()
                || !r.packages.iter().all(valid_package)
        })
    {
        return Err(
            AppError::new("marketProfileInvalid").detail("invalid marketplace installation record")
        );
    }
    Ok(receipts)
}

pub(super) fn write_receipts(profile: &Path, receipts: &InstallReceipts) -> AppResult<()> {
    crate::paths::atomic_write(
        &profile.join(RECEIPTS_FILE),
        &serde_json::to_vec_pretty(receipts)?,
    )
}

/// Only dependencies introduced by the market are eligible for cleanup.
/// Repeatedly retain providers needed by any surviving direct dependency or
/// bundle, including providers needed by another retained provider.
pub(super) fn group_removals(
    profile: &Path,
    manifest: &ProfileManifest,
    receipts: &InstallReceipts,
    plugin_id: &str,
) -> AppResult<Vec<String>> {
    let claimed: HashSet<_> = receipts
        .plugins
        .iter()
        .filter(|(id, _)| *id != plugin_id)
        .flat_map(|(_, r)| r.packages.iter().cloned())
        .collect();
    let mut removable: HashSet<_> = receipts
        .managed_packages
        .iter()
        .filter(|p| manifest.dependencies.contains_key(*p) && !claimed.contains(*p))
        .cloned()
        .collect();
    loop {
        let mut required = HashSet::new();
        for package in manifest.dependencies.keys().chain(&manifest.bundles) {
            if removable.contains(package) {
                continue;
            }
            if manifest.dependencies.contains_key(package)
                && !profile
                    .join("node_modules")
                    .join(package)
                    .join("package.json")
                    .is_file()
            {
                // Missing metadata cannot prove that a surviving dependency
                // no longer needs a provider. Keep managed packages until the
                // profile can be repaired, while still allowing receipt removal.
                removable.clear();
                return Ok(Vec::new());
            }
            for dependency in package_requirements(profile, package)? {
                if removable.contains(&dependency) {
                    required.insert(dependency);
                }
            }
        }
        if required.is_empty() {
            break;
        }
        removable.retain(|p| !required.contains(p));
    }
    let mut result: Vec<_> = removable.into_iter().collect();
    result.sort();
    Ok(result)
}

pub(super) fn package_requirements(profile: &Path, package: &str) -> AppResult<Vec<String>> {
    let path = profile
        .join("node_modules")
        .join(package)
        .join("package.json");
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let package: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| {
        AppError::new("marketProfileInvalid")
            .detail(format!("cannot read dependency requirements: {e}"))
    })?;
    Ok(["dependencies", "peerDependencies", "optionalDependencies"]
        .into_iter()
        .filter_map(|field| package.get(field).and_then(serde_json::Value::as_object))
        .flat_map(|requirements| requirements.keys().cloned())
        .collect())
}
