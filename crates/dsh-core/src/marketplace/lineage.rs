//! Publication-only trust verification. GitHub's compare API also computes a
//! file diff, which can return HTTP 422 as the catalog history grows. Fetch
//! only the commit graph and check ancestry locally instead.

use super::{
    AppError, AppResult, Command, Duration, MARKET_REPOSITORY, MARKET_TRUST_ANCHOR, Path, Stdio,
    new_command, run_child, valid_commit_sha,
};

const GIT_TIMEOUT: Duration = Duration::from_secs(180);

pub(super) fn verify_catalog_lineage(head: &str) -> AppResult<()> {
    if !valid_commit_sha(head) {
        return Err(AppError::new("marketCatalogInvalid").detail("catalog commit is invalid"));
    }
    if head == MARKET_TRUST_ANCHOR {
        return Ok(());
    }
    let repository =
        tempfile::tempdir().map_err(|error| AppError::io("marketCatalogInvalid", &error))?;
    checked_git(repository.path(), &["init", "--bare", "--template=", "."])?;
    let remote = format!("https://github.com/{MARKET_REPOSITORY}.git");
    // No depth limit: missing ancestors must never be mistaken for a trusted
    // history. No checkout, blobs, trees, tags, submodules or upstream code.
    checked_git(
        repository.path(),
        &[
            "fetch",
            "--filter=tree:0",
            "--no-tags",
            "--no-recurse-submodules",
            &remote,
            head,
        ],
    )?;
    verify_ancestry(repository.path(), MARKET_TRUST_ANCHOR, head)
}

fn git_command(repository: &Path) -> Command {
    let mut command = new_command("git");
    // Never inherit credentials, injected Git configuration, alternate object
    // databases or replacement refs from the host/checkout. This public fetch
    // needs no token or credential helper, including on macOS.
    let allowed = [
        "PATH",
        "SYSTEMROOT",
        "TEMP",
        "TMP",
        "TMPDIR",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
    ];
    let environment: Vec<_> = allowed
        .iter()
        .filter_map(|key| std::env::var_os(key).map(|value| (*key, value)))
        .collect();
    command
        .env_clear()
        .envs(environment)
        .env("HOME", repository)
        .env("USERPROFILE", repository)
        .env("XDG_CONFIG_HOME", repository)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            repository.join("disabled-global-config"),
        )
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .args(["-c", "credential.helper=", "-c", "core.askPass=", "-C"])
        .arg(repository)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::network::apply_to_command(&mut command);
    command
}

fn checked_git(repository: &Path, arguments: &[&str]) -> AppResult<String> {
    let mut command = git_command(repository);
    command.args(arguments);
    let (success, output) = run_child(command, GIT_TIMEOUT)?;
    if !success {
        return Err(AppError::new("marketCatalogInvalid").detail(format!(
            "catalog history {} failed: {}",
            arguments[0],
            crate::network::sanitize_detail(&output)
        )));
    }
    Ok(output)
}

fn verify_ancestry(repository: &Path, anchor: &str, head: &str) -> AppResult<()> {
    if checked_git(repository, &["rev-parse", "--is-shallow-repository"])?.trim() != "false" {
        return Err(AppError::new("marketCatalogInvalid").detail("catalog history is shallow"));
    }
    checked_git(repository, &["merge-base", "--is-ancestor", anchor, head])
        .map(|_| ())
        .map_err(|error| {
            AppError::new("marketCatalogInvalid").detail(format!(
                "catalog history does not prove descent from the trusted release anchor: {error}"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn commit(repository: &Path, tree: &str, parents: &[&str]) -> String {
        static NEXT_COMMIT: AtomicUsize = AtomicUsize::new(0);
        let message = format!("synthetic {}", NEXT_COMMIT.fetch_add(1, Ordering::Relaxed));
        let mut command = git_command(repository);
        command
            .env("GIT_AUTHOR_NAME", "Synthetic test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Synthetic test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .args(["commit-tree", tree, "-m", &message]);
        for parent in parents {
            command.args(["-p", parent]);
        }
        let (success, output) = run_child(command, GIT_TIMEOUT).expect("commit-tree");
        assert!(success, "{output}");
        output.trim().to_owned()
    }

    #[test]
    fn catalog_lineage_accepts_descendants_and_merges_but_rejects_rewritten_history() {
        let repository = tempfile::tempdir().expect("isolated repository");
        let path = repository.path();
        checked_git(path, &["init", "--bare", "--template=", "."]).expect("init");
        let tree = checked_git(path, &["mktree"]).expect("empty tree");
        let tree = tree.trim();
        let root = commit(path, tree, &[]);
        let anchor = commit(path, tree, &[&root]);
        let head = commit(path, tree, &[&anchor]);
        let sibling = commit(path, tree, &[&root]);
        let merged = commit(path, tree, &[&sibling, &head]);
        verify_ancestry(path, &anchor, &head).expect("descendant");
        verify_ancestry(path, &anchor, &merged).expect("merge descendant");
        verify_ancestry(path, &anchor, &anchor).expect("same commit");
        assert!(
            verify_ancestry(path, &anchor, &sibling).is_err(),
            "diverged history"
        );
        assert!(
            verify_ancestry(path, &anchor, &root).is_err(),
            "rollback before anchor"
        );
        assert!(
            verify_ancestry(path, &head, &anchor).is_err(),
            "reverse ancestry"
        );
        let missing = "1111111111111111111111111111111111111111";
        assert!(
            verify_ancestry(path, missing, &head).is_err(),
            "missing anchor"
        );
        assert!(
            verify_ancestry(path, &anchor, missing).is_err(),
            "missing head"
        );
        std::fs::write(path.join("shallow"), format!("{head}\n")).expect("shallow boundary");
        assert!(
            verify_ancestry(path, &anchor, &head).is_err(),
            "incomplete history"
        );
    }

    #[test]
    fn catalog_lineage_verifies_a_filtered_fetch_without_trees_or_blobs() {
        let source = tempfile::tempdir().expect("synthetic upstream");
        checked_git(source.path(), &["init", "--bare", "--template=", "."]).expect("init source");
        checked_git(source.path(), &["config", "uploadpack.allowFilter", "true"]).expect("filter");
        let fixture = source.path().join("synthetic-catalog.json");
        std::fs::write(&fixture, br#"{"plugins":[]}"#).expect("synthetic catalog");
        let blob = checked_git(
            source.path(),
            &["hash-object", "-w", fixture.to_str().unwrap()],
        )
        .expect("catalog blob");
        checked_git(source.path(), &["read-tree", "--empty"]).expect("empty index");
        checked_git(
            source.path(),
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                "100644",
                blob.trim(),
                "data/plugins.json",
            ],
        )
        .expect("index catalog");
        let tree = checked_git(source.path(), &["write-tree"]).expect("catalog tree");
        let anchor = commit(source.path(), tree.trim(), &[]);
        let head = commit(source.path(), tree.trim(), &[&anchor]);
        checked_git(source.path(), &["update-ref", "refs/heads/master", &head])
            .expect("source head");
        let destination = tempfile::tempdir().expect("isolated destination");
        checked_git(destination.path(), &["init", "--bare", "--template=", "."])
            .expect("init destination");
        let remote = url::Url::from_directory_path(source.path()).expect("file URL");
        checked_git(
            destination.path(),
            &[
                "fetch",
                "--filter=tree:0",
                "--no-tags",
                "--no-recurse-submodules",
                remote.as_str(),
                &head,
            ],
        )
        .expect("filtered fetch");
        verify_ancestry(destination.path(), &anchor, &head).expect("complete commit graph");
        // --missing=print reports missing objects without lazy fetching them.
        let objects = checked_git(
            destination.path(),
            &["rev-list", "--objects", "--missing=print", &head],
        )
        .expect("inspect filtered objects");
        assert!(
            objects.contains(&format!("?{}", tree.trim())),
            "history verification must not require tree objects: {objects}"
        );
    }

    #[test]
    fn catalog_lineage_rejects_invalid_heads_before_fetching() {
        assert!(verify_catalog_lineage("--upload-pack=untrusted").is_err());
        assert!(verify_catalog_lineage("master").is_err());
        verify_catalog_lineage(MARKET_TRUST_ANCHOR).expect("embedded anchor");
    }
}
