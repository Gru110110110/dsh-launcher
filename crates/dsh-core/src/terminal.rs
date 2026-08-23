//! Installs a stable `dsh` command that delegates to the launcher's private
//! Node.js and Harness runtime. The command lives inside the application home;
//! only a small, owned PATH entry is added to the user's shell environment.

use std::{fs, path::Path};

use crate::{
    AppError, AppResult,
    paths::{ApplicationPaths, atomic_write, dirs_home},
};

#[cfg(unix)]
const PROFILE_START: &str = "# >>> DSH Launcher terminal command >>>";
#[cfg(unix)]
const PROFILE_END: &str = "# <<< DSH Launcher terminal command <<<";
#[cfg(unix)]
const UNIX_WRAPPER_PREFIX: &str =
    "#!/bin/sh\n# Managed by DSH Launcher; manual changes may be replaced.\n";
#[cfg(windows)]
const WINDOWS_WRAPPER_PREFIX: &str =
    "@echo off\r\nrem Managed by DSH Launcher; manual changes may be replaced.\r\n";

/// Creates or refreshes the application-owned command wrapper. When
/// `configure_user_path` is true, the default, non-overridden application home
/// is also appended to the user's PATH for subsequently opened terminals.
pub fn ensure_terminal_command(
    paths: &ApplicationPaths,
    configure_user_path: bool,
) -> AppResult<()> {
    ensure_runtime_available(paths)?;
    ensure_owned_bin_directory(&paths.terminal_bin_dir)?;
    write_command_wrapper(paths)?;
    if !configure_user_path {
        return Ok(());
    }
    configure_user_path_entry(paths)
}

fn ensure_runtime_available(paths: &ApplicationPaths) -> AppResult<()> {
    for path in [&paths.node_bin, &paths.dsh_bin] {
        let metadata = fs::symlink_metadata(path).map_err(|error| {
            AppError::io("terminalRuntimeUnavailable", &error).value("path", path.display())
        })?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(AppError::new("terminalRuntimeUnavailable").value("path", path.display()));
        }
    }
    Ok(())
}

fn ensure_owned_bin_directory(path: &Path) -> AppResult<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(AppError::new("terminalBinDirectoryInvalid").value("path", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path).map_err(|error| {
                AppError::io("terminalBinDirectoryCreateFailed", &error)
                    .value("path", path.display())
            })
        }
        Err(error) => {
            Err(AppError::io("terminalBinDirectoryInvalid", &error).value("path", path.display()))
        }
    }
}

fn ensure_owned_wrapper(path: &Path, owned_prefix: &[u8]) -> AppResult<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(
                AppError::io("terminalCommandInspectFailed", &error).value("path", path.display())
            );
        }
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(AppError::new("terminalCommandConflict").value("path", path.display()));
    }
    let existing = fs::read(path).map_err(|error| {
        AppError::io("terminalCommandInspectFailed", &error).value("path", path.display())
    })?;
    if !existing.starts_with(owned_prefix) {
        return Err(AppError::new("terminalCommandConflict").value("path", path.display()));
    }
    Ok(())
}

#[cfg(any(windows, test))]
fn append_path_value(current: &str, bin: &str, separator: char) -> String {
    let normalized_bin = normalize_path_entry(bin, separator);
    if current
        .split(separator)
        .map(|entry| normalize_path_entry(entry, separator))
        .any(|entry| {
            if separator == ';' {
                entry.eq_ignore_ascii_case(&normalized_bin)
            } else {
                entry == normalized_bin
            }
        })
    {
        return current.to_owned();
    }
    let mut updated = current.trim_end_matches(separator).to_owned();
    if !updated.is_empty() {
        updated.push(separator);
    }
    updated.push_str(bin);
    updated
}

#[cfg(any(windows, test))]
fn normalize_path_entry(value: &str, separator: char) -> String {
    let value = value.trim();
    let value = if separator == ';' {
        value.trim_matches('"')
    } else {
        value
    };
    value.trim_end_matches(['\\', '/']).to_owned()
}

#[cfg(unix)]
fn write_command_wrapper(paths: &ApplicationPaths) -> AppResult<()> {
    use std::os::unix::fs::PermissionsExt;

    ensure_owned_wrapper(&paths.terminal_dsh_bin, UNIX_WRAPPER_PREFIX.as_bytes())?;
    let dsh_home = shell_quote(&paths.dsh_home)?;
    let node_bin = shell_quote(&paths.node_bin)?;
    let dsh_bin = shell_quote(&paths.dsh_bin)?;
    let script = format!(
        "{UNIX_WRAPPER_PREFIX}if [ \"${{DSH_HOME+x}}\" != x ]; then\n  export DSH_HOME={}\nfi\nexec {} {} \"$@\"\n",
        dsh_home, node_bin, dsh_bin,
    );
    atomic_write(&paths.terminal_dsh_bin, script.as_bytes()).map_err(|error| {
        AppError::new("terminalCommandWriteFailed")
            .value("path", paths.terminal_dsh_bin.display())
            .detail(error.to_string())
    })?;
    fs::set_permissions(&paths.terminal_dsh_bin, fs::Permissions::from_mode(0o755)).map_err(
        |error| {
            AppError::io("terminalCommandPermissionFailed", &error)
                .value("path", paths.terminal_dsh_bin.display())
        },
    )
}

#[cfg(windows)]
fn write_command_wrapper(paths: &ApplicationPaths) -> AppResult<()> {
    ensure_owned_wrapper(&paths.terminal_dsh_bin, WINDOWS_WRAPPER_PREFIX.as_bytes())?;
    let script = format!(
        "{WINDOWS_WRAPPER_PREFIX}if not defined DSH_HOME set \"DSH_HOME=%~dp0..\\dsh-home\"\r\n\"%~dp0..\\runtime\\node\\node.exe\" \"%~dp0..\\runtime\\dsh\\node_modules\\@deepseek-ai\\dsh\\lib\\bin.js\" %*\r\n"
    );
    atomic_write(&paths.terminal_dsh_bin, script.as_bytes()).map_err(|error| {
        AppError::new("terminalCommandWriteFailed")
            .value("path", paths.terminal_dsh_bin.display())
            .detail(error.to_string())
    })
}

#[cfg(unix)]
fn shell_quote(path: &Path) -> AppResult<String> {
    let value = path
        .to_str()
        .ok_or_else(|| AppError::new("terminalPathInvalid").value("path", path.display()))?;
    Ok(format!("'{}'", value.replace('\'', "'\\''")))
}

#[cfg(unix)]
fn configure_user_path_entry(paths: &ApplicationPaths) -> AppResult<()> {
    let home = dirs_home()?;
    if paths.app_home != home.join(".dsh-desktop") {
        return Err(AppError::new("terminalHomeMismatch"));
    }
    let shell_value = std::env::var_os("SHELL");
    let shell = shell_value
        .as_deref()
        .and_then(|shell| Path::new(shell).file_name())
        .and_then(|name| name.to_str());
    let mut first_error = None;
    for profile in unix_shell_profiles(&home, shell) {
        if let Err(error) = update_unix_profile(&profile, &paths.terminal_bin_dir)
            && first_error.is_none()
        {
            first_error = Some(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

#[cfg(unix)]
fn unix_shell_profiles(home: &Path, shell: Option<&str>) -> Vec<std::path::PathBuf> {
    match shell {
        Some("bash") => vec![home.join(".bash_profile"), home.join(".bashrc")],
        Some("sh" | "ksh") => vec![home.join(".profile")],
        Some("zsh") => vec![home.join(".zprofile"), home.join(".zshrc")],
        #[cfg(target_os = "macos")]
        _ => vec![home.join(".zprofile"), home.join(".zshrc")],
        #[cfg(not(target_os = "macos"))]
        _ => vec![home.join(".profile")],
    }
}

#[cfg(unix)]
fn update_unix_profile(profile: &Path, bin_dir: &Path) -> AppResult<()> {
    let (target, existing, permissions) = match fs::symlink_metadata(profile) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            let bytes = fs::read(profile).map_err(|error| {
                AppError::io("terminalProfileReadFailed", &error).value("path", profile.display())
            })?;
            let contents = String::from_utf8(bytes).map_err(|error| {
                AppError::new("terminalProfileInvalid")
                    .value("path", profile.display())
                    .detail(error.to_string())
            })?;
            (profile.to_owned(), contents, Some(metadata.permissions()))
        }
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let target = fs::canonicalize(profile).map_err(|error| {
                AppError::io("terminalProfileReadFailed", &error).value("path", profile.display())
            })?;
            let profile_home = profile
                .parent()
                .and_then(|parent| fs::canonicalize(parent).ok())
                .ok_or_else(|| {
                    AppError::new("terminalProfileInvalid").value("path", profile.display())
                })?;
            if !target.starts_with(&profile_home) {
                return Err(
                    AppError::new("terminalProfileInvalid").value("path", profile.display())
                );
            }
            let target_metadata = fs::symlink_metadata(&target).map_err(|error| {
                AppError::io("terminalProfileReadFailed", &error).value("path", profile.display())
            })?;
            if !target_metadata.is_file() || target_metadata.file_type().is_symlink() {
                return Err(
                    AppError::new("terminalProfileInvalid").value("path", profile.display())
                );
            }
            let bytes = fs::read(&target).map_err(|error| {
                AppError::io("terminalProfileReadFailed", &error).value("path", profile.display())
            })?;
            let contents = String::from_utf8(bytes).map_err(|error| {
                AppError::new("terminalProfileInvalid")
                    .value("path", profile.display())
                    .detail(error.to_string())
            })?;
            (target, contents, Some(target_metadata.permissions()))
        }
        Ok(_) => {
            return Err(AppError::new("terminalProfileInvalid").value("path", profile.display()));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            (profile.to_owned(), String::new(), None)
        }
        Err(error) => {
            return Err(
                AppError::io("terminalProfileReadFailed", &error).value("path", profile.display())
            );
        }
    };
    let updated = profile_with_managed_path(&existing, bin_dir)?;
    if updated == existing {
        return Ok(());
    }
    atomic_write(&target, updated.as_bytes()).map_err(|error| {
        AppError::new("terminalProfileWriteFailed")
            .value("path", profile.display())
            .detail(error.to_string())
    })?;
    if let Some(permissions) = permissions {
        fs::set_permissions(&target, permissions).map_err(|error| {
            AppError::io("terminalProfilePermissionFailed", &error).value("path", profile.display())
        })?;
    }
    Ok(())
}

#[cfg(unix)]
fn profile_with_managed_path(existing: &str, bin_dir: &Path) -> AppResult<String> {
    let starts = exact_line_offsets(existing, PROFILE_START);
    let ends = exact_line_offsets(existing, PROFILE_END);
    if starts.len() > 1 || ends.len() > 1 || starts.len() != ends.len() {
        return Err(AppError::new("terminalProfileManagedBlockInvalid"));
    }
    let quoted = shell_quote(bin_dir)?;
    let block = format!(
        "{PROFILE_START}\n_DSH_LAUNCHER_BIN={quoted}\ncase \":${{PATH:-}}:\" in\n  *\":${{_DSH_LAUNCHER_BIN}}:\"*) ;;\n  *) export PATH=\"${{PATH:+$PATH:}}${{_DSH_LAUNCHER_BIN}}\" ;;\nesac\nunset _DSH_LAUNCHER_BIN\n{PROFILE_END}"
    );
    match (starts.first(), ends.first()) {
        (Some((start, _)), Some((end, after_end))) if start <= end => Ok(format!(
            "{}{}{}",
            &existing[..*start],
            block,
            &existing[*after_end..]
        )),
        (None, None) => {
            let mut updated = existing.to_owned();
            if !updated.is_empty() && !updated.ends_with('\n') {
                updated.push('\n');
            }
            updated.push_str(&block);
            updated.push('\n');
            Ok(updated)
        }
        _ => Err(AppError::new("terminalProfileManagedBlockInvalid")),
    }
}

#[cfg(unix)]
fn exact_line_offsets(contents: &str, expected: &str) -> Vec<(usize, usize)> {
    let mut offset = 0;
    let mut matches = Vec::new();
    for segment in contents.split_inclusive('\n') {
        let content_length = segment.strip_suffix('\n').map_or(segment.len(), str::len);
        let line = &segment[..content_length];
        if line.strip_suffix('\r').unwrap_or(line) == expected {
            matches.push((offset, offset + content_length));
        }
        offset += segment.len();
    }
    matches
}

#[cfg(windows)]
fn configure_user_path_entry(paths: &ApplicationPaths) -> AppResult<()> {
    let home = dirs_home()?;
    if paths.app_home != home.join(".dsh-desktop") {
        return Err(AppError::new("terminalHomeMismatch"));
    }
    windows_path::append_user_path(&paths.terminal_bin_dir)
}

#[cfg(windows)]
mod windows_path {
    use std::{ffi::OsStr, os::windows::ffi::OsStrExt, path::Path, ptr};

    use windows_sys::Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS},
        System::Registry::{
            HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_EXPAND_SZ,
            REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey, RegCreateKeyExW, RegQueryValueExW,
            RegSetValueExW,
        },
        UI::WindowsAndMessaging::{
            HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
        },
    };

    use super::append_path_value;
    use crate::{AppError, AppResult};

    struct RegistryKey(HKEY);

    impl Drop for RegistryKey {
        fn drop(&mut self) {
            unsafe {
                RegCloseKey(self.0);
            }
        }
    }

    pub(super) fn append_user_path(bin_dir: &Path) -> AppResult<()> {
        let subkey = wide("Environment");
        let value_name = wide("Path");
        let mut raw_key = ptr::null_mut();
        let status = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                0,
                ptr::null_mut(),
                REG_OPTION_NON_VOLATILE,
                KEY_QUERY_VALUE | KEY_SET_VALUE,
                ptr::null(),
                &mut raw_key,
                ptr::null_mut(),
            )
        };
        check_status("terminalPathRegistryOpenFailed", status)?;
        let key = RegistryKey(raw_key);
        let (current, value_type) = read_path(key.0, &value_name)?;
        let bin = bin_dir.to_string_lossy();
        let updated = append_path_value(&current, &bin, ';');
        if updated == current {
            // Retry the notification on later launches in case Explorer did
            // not acknowledge it immediately after the registry write.
            broadcast_environment_change();
            return Ok(());
        }
        let encoded: Vec<u16> = OsStr::new(&updated).encode_wide().chain(Some(0)).collect();
        if encoded.len() > 32_767 {
            return Err(AppError::new("terminalPathRegistryValueTooLarge"));
        }
        let status = unsafe {
            RegSetValueExW(
                key.0,
                value_name.as_ptr(),
                0,
                value_type,
                encoded.as_ptr().cast(),
                u32::try_from(encoded.len() * size_of::<u16>())
                    .map_err(|_| AppError::new("terminalPathRegistryValueTooLarge"))?,
            )
        };
        check_status("terminalPathRegistryWriteFailed", status)?;
        broadcast_environment_change();
        Ok(())
    }

    fn read_path(key: HKEY, value_name: &[u16]) -> AppResult<(String, u32)> {
        let mut value_type = 0;
        let mut byte_count = 0;
        let status = unsafe {
            RegQueryValueExW(
                key,
                value_name.as_ptr(),
                ptr::null(),
                &mut value_type,
                ptr::null_mut(),
                &mut byte_count,
            )
        };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok((String::new(), REG_EXPAND_SZ));
        }
        check_status("terminalPathRegistryReadFailed", status)?;
        if value_type != REG_SZ && value_type != REG_EXPAND_SZ {
            return Err(AppError::new("terminalPathRegistryTypeInvalid"));
        }
        if byte_count % 2 != 0 {
            return Err(AppError::new("terminalPathRegistryValueInvalid"));
        }
        let mut bytes = vec![0_u8; byte_count as usize];
        let status = unsafe {
            RegQueryValueExW(
                key,
                value_name.as_ptr(),
                ptr::null(),
                &mut value_type,
                bytes.as_mut_ptr(),
                &mut byte_count,
            )
        };
        check_status("terminalPathRegistryReadFailed", status)?;
        let mut words: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect();
        while words.last() == Some(&0) {
            words.pop();
        }
        let value = String::from_utf16(&words).map_err(|error| {
            AppError::new("terminalPathRegistryValueInvalid").detail(error.to_string())
        })?;
        Ok((value, value_type))
    }

    fn broadcast_environment_change() {
        let environment = wide("Environment");
        let mut result = 0;
        let delivered = unsafe {
            SendMessageTimeoutW(
                HWND_BROADCAST,
                WM_SETTINGCHANGE,
                0,
                environment.as_ptr() as isize,
                SMTO_ABORTIFHUNG,
                1_000,
                &mut result,
            )
        };
        if delivered == 0 {
            log::warn!("Windows did not acknowledge the user PATH change notification");
        }
    }

    fn wide(value: &str) -> Vec<u16> {
        OsStr::new(value).encode_wide().chain(Some(0)).collect()
    }

    fn check_status(code: &'static str, status: u32) -> AppResult<()> {
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(AppError::io(
                code,
                &std::io::Error::from_raw_os_error(status as i32),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::{
        os::unix::fs::{PermissionsExt, symlink},
        process::Command,
    };

    #[test]
    fn windows_path_append_is_case_insensitive_and_preserves_existing_order() {
        assert_eq!(
            append_path_value(
                r"C:\Tools;C:\Users\Me\DSH\BIN\\",
                r"c:\users\me\dsh\bin",
                ';'
            ),
            r"C:\Tools;C:\Users\Me\DSH\BIN\\"
        );
        assert_eq!(
            append_path_value(r"C:\Tools;;", r"C:\DSH\bin", ';'),
            r"C:\Tools;C:\DSH\bin"
        );
    }

    #[test]
    fn path_append_does_not_create_an_empty_search_entry() {
        assert_eq!(append_path_value("", "/dsh/bin", ':'), "/dsh/bin");
    }

    #[cfg(unix)]
    #[test]
    fn interactive_and_login_profiles_are_both_configured() {
        let home = Path::new("/user");
        assert_eq!(
            unix_shell_profiles(home, Some("zsh")),
            vec![home.join(".zprofile"), home.join(".zshrc")]
        );
        assert_eq!(
            unix_shell_profiles(home, Some("bash")),
            vec![home.join(".bash_profile"), home.join(".bashrc")]
        );
    }

    #[cfg(unix)]
    fn fake_runtime(paths: &ApplicationPaths) {
        fs::create_dir_all(paths.node_bin.parent().unwrap()).unwrap();
        fs::create_dir_all(paths.dsh_bin.parent().unwrap()).unwrap();
        fs::write(
            &paths.node_bin,
            b"#!/bin/sh\nprintf '%s\\n' \"$DSH_HOME|$1|$2\"\n",
        )
        .unwrap();
        fs::set_permissions(&paths.node_bin, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(&paths.dsh_bin, b"fake dsh").unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn wrapper_uses_private_runtime_and_default_home() {
        let temp = tempfile::tempdir().unwrap();
        let paths = ApplicationPaths::from_home(temp.path().join("desktop home"));
        fake_runtime(&paths);

        ensure_terminal_command(&paths, false).unwrap();
        let output = Command::new(&paths.terminal_dsh_bin)
            .args(["--version", "extra"])
            .env_remove("DSH_HOME")
            .output()
            .unwrap();

        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            format!(
                "{}|{}|--version",
                paths.dsh_home.display(),
                paths.dsh_bin.display()
            )
        );
        assert_eq!(
            fs::metadata(&paths.terminal_dsh_bin)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
    }

    #[cfg(unix)]
    #[test]
    fn wrapper_preserves_an_explicit_dsh_home() {
        let temp = tempfile::tempdir().unwrap();
        let paths = ApplicationPaths::from_home(temp.path().join("desktop"));
        fake_runtime(&paths);
        ensure_terminal_command(&paths, false).unwrap();

        let output = Command::new(&paths.terminal_dsh_bin)
            .arg("--version")
            .env("DSH_HOME", "explicit-home")
            .output()
            .unwrap();

        assert!(
            String::from_utf8(output.stdout)
                .unwrap()
                .starts_with("explicit-home|")
        );
    }

    #[cfg(unix)]
    #[test]
    fn profile_update_is_idempotent_and_preserves_surrounding_content() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".zprofile");
        fs::write(&profile, "before\nafter").unwrap();
        let bin = temp.path().join("desktop home's bin");

        update_unix_profile(&profile, &bin).unwrap();
        let once = fs::read_to_string(&profile).unwrap();
        update_unix_profile(&profile, &bin).unwrap();
        let twice = fs::read_to_string(&profile).unwrap();

        assert_eq!(once, twice);
        assert!(once.starts_with("before\nafter\n"));
        assert_eq!(once.matches(PROFILE_START).count(), 1);
        assert!(once.contains("desktop home'\\''s bin"));
    }

    #[cfg(unix)]
    #[test]
    fn profile_update_replaces_only_its_managed_block() {
        let existing = format!("keep\n{PROFILE_START}\nold\n{PROFILE_END}\nstill keep\n");
        let updated = profile_with_managed_path(&existing, Path::new("/new/bin")).unwrap();

        assert!(updated.starts_with("keep\n"));
        assert!(updated.ends_with("\nstill keep\n"));
        assert!(updated.contains("_DSH_LAUNCHER_BIN='/new/bin'"));
        assert!(!updated.contains("\nold\n"));
    }

    #[cfg(unix)]
    #[test]
    fn profile_markers_must_occupy_their_own_lines() {
        let existing = format!("echo '{PROFILE_START}'\necho '{PROFILE_END}'\n");
        let updated = profile_with_managed_path(&existing, Path::new("/new/bin")).unwrap();

        assert!(updated.starts_with(&existing));
        assert_eq!(updated.matches(PROFILE_START).count(), 2);
        assert_eq!(exact_line_offsets(&updated, PROFILE_START).len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn profile_symlinks_are_preserved_while_their_target_is_updated() {
        let temp = tempfile::tempdir().unwrap();
        let dotfiles = temp.path().join("dotfiles");
        fs::create_dir(&dotfiles).unwrap();
        let target = dotfiles.join("zprofile");
        fs::write(&target, "# user config\n").unwrap();
        let profile = temp.path().join(".zprofile");
        symlink(&target, &profile).unwrap();

        update_unix_profile(&profile, Path::new("/dsh/bin")).unwrap();

        assert!(
            fs::symlink_metadata(&profile)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let updated = fs::read_to_string(&target).unwrap();
        assert!(updated.starts_with("# user config\n"));
        assert_eq!(exact_line_offsets(&updated, PROFILE_START).len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn profile_symlinks_cannot_redirect_writes_outside_the_user_home() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        fs::create_dir(&home).unwrap();
        let outside = temp.path().join("outside-profile");
        fs::write(&outside, "outside\n").unwrap();
        let profile = home.join(".zprofile");
        symlink(&outside, &profile).unwrap();

        let error = update_unix_profile(&profile, Path::new("/dsh/bin")).unwrap_err();

        assert_eq!(error.code, "terminalProfileInvalid");
        assert_eq!(fs::read_to_string(&outside).unwrap(), "outside\n");
        assert!(
            fs::symlink_metadata(&profile)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(unix)]
    #[test]
    fn managed_profile_block_is_valid_shell_and_avoids_duplicate_entries() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".profile");
        let bin = temp.path().join("desktop home's bin");
        update_unix_profile(&profile, &bin).unwrap();

        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg(". ./.profile; . ./.profile; printf '%s' \"$PATH\"")
            .current_dir(temp.path())
            .env("PATH", "/usr/bin")
            .output()
            .unwrap();

        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!("/usr/bin:{}", bin.display())
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_unowned_command_is_never_overwritten() {
        let temp = tempfile::tempdir().unwrap();
        let paths = ApplicationPaths::from_home(temp.path().join("desktop"));
        fake_runtime(&paths);
        fs::create_dir_all(&paths.terminal_bin_dir).unwrap();
        fs::write(
            &paths.terminal_dsh_bin,
            "user command\n# Managed by DSH Launcher; manual changes may be replaced.\n",
        )
        .unwrap();

        let error = ensure_terminal_command(&paths, false).unwrap_err();

        assert_eq!(error.code, "terminalCommandConflict");
        assert_eq!(
            fs::read_to_string(&paths.terminal_dsh_bin).unwrap(),
            "user command\n# Managed by DSH Launcher; manual changes may be replaced.\n"
        );
    }
}
