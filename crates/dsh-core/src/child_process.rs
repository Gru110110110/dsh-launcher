use std::{ffi::OsStr, process::Command};

/// Create a subprocess with the launcher's platform-wide visibility policy.
///
/// Windows console executables allocate a visible console unless every call
/// site remembers to opt out. Keeping that policy in the constructor makes
/// hidden execution the default for services, probes, installers, browsers,
/// tests, and future subprocesses alike.
#[allow(clippy::disallowed_methods)]
pub fn new_command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    hide_console_window(&mut command);
    command
}

#[cfg(windows)]
fn hide_console_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;

    command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_console_window(_command: &mut Command) {}

#[cfg(unix)]
pub(crate) fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    command.process_group(0);
}

#[cfg(windows)]
pub(crate) fn configure_process_group(command: &mut Command) {
    use std::os::windows::process::CommandExt;

    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(
        CREATE_NEW_PROCESS_GROUP | windows_sys::Win32::System::Threading::CREATE_NO_WINDOW,
    );
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    #[cfg(windows)]
    use super::*;

    #[test]
    fn windows_process_creation_is_centralized() {
        let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let workspace_root = crate_root
            .parent()
            .and_then(Path::parent)
            .expect("dsh-core must live under the workspace crates directory");
        let policy_file = Path::new(file!());
        let mut violations = Vec::new();

        // Inspect source roots only. Tests must not traverse unrelated
        // workspace directories that may contain signing material or other
        // operator-owned files.
        for root in [
            workspace_root.join("crates"),
            workspace_root.join("src-tauri"),
        ] {
            collect_direct_command_construction(&root, policy_file, &mut violations);
        }

        assert!(
            violations.is_empty(),
            "subprocesses must be created with child_process::new_command so Windows never opens a console; direct Command::new found at:\n{}",
            violations.join("\n")
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_child_command_does_not_allocate_a_console() {
        use std::process::Stdio;

        let mut child = new_command(std::env::current_exe().expect("test executable"))
            .args([
                "--ignored",
                "--exact",
                "child_process::tests::windows_no_console_helper",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("hidden child process must start");
        let status = child.wait().expect("hidden child process must exit");
        assert!(status.success(), "hidden child process reported a console");
    }

    #[cfg(windows)]
    #[test]
    #[ignore]
    fn windows_no_console_helper() {
        let window = unsafe { windows_sys::Win32::System::Console::GetConsoleWindow() };
        assert!(window.is_null(), "child process inherited a console window");
    }

    fn collect_direct_command_construction(
        directory: &Path,
        policy_file: &Path,
        violations: &mut Vec<String>,
    ) {
        let mut entries = fs::read_dir(directory)
            .unwrap_or_else(|error| panic!("could not inspect {}: {error}", directory.display()))
            .collect::<Result<Vec<_>, _>>()
            .unwrap_or_else(|error| panic!("could not inspect {}: {error}", directory.display()));
        entries.sort_by_key(|entry| entry.path());
        for entry in entries {
            let path = entry.path();
            let file_type = entry
                .file_type()
                .unwrap_or_else(|error| panic!("could not inspect {}: {error}", path.display()));
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                if matches!(
                    entry.file_name().to_str(),
                    Some(".git" | "node_modules" | "target")
                ) {
                    continue;
                }
                collect_direct_command_construction(&path, policy_file, violations);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                inspect_source(&path, policy_file, violations);
            }
        }
    }

    fn inspect_source(path: &Path, policy_file: &Path, violations: &mut Vec<String>) {
        if path.ends_with(policy_file) {
            return;
        }
        let source = fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("could not inspect {}: {error}", path.display()));
        for (index, line) in source.lines().enumerate() {
            if line.contains("Command::new(") || line.contains("std::process::Command::new(") {
                violations.push(format!("{}:{}", path.display(), index + 1));
            }
        }
    }
}
