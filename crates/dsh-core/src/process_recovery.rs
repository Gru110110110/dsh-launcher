use std::{
    collections::BTreeSet,
    fs, thread,
    time::{Duration, Instant},
};

#[cfg(target_os = "macos")]
use std::{
    ffi::OsString,
    mem::{size_of, zeroed},
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
};

#[cfg(windows)]
use std::{
    ffi::{OsStr, OsString, c_void},
    mem::size_of,
    os::windows::ffi::OsStringExt,
    path::{Path, PathBuf},
    ptr,
};

use crate::{AppError, AppResult, ApplicationPaths};

pub(crate) const SERVICE_GUARD_ARGUMENT: &str = "--dsh-service-guard";
pub(crate) const SERVICE_GUARD_HOME_ARGUMENT: &str = "--desktop-home";
pub(crate) const SERVICE_GUARD_FREE_PORT_ARGUMENT: &str = "--free-port";
#[cfg(unix)]
const RECOVERY_TERM_TIMEOUT: Duration = Duration::from_secs(3);
#[cfg(any(unix, windows))]
const RECOVERY_KILL_TIMEOUT: Duration = Duration::from_secs(5);
const RECOVERY_SCAN_TIMEOUT: Duration = Duration::from_secs(60);
#[cfg(target_os = "macos")]
const RECOVERY_IDENTITY_SETTLE_TIMEOUT: Duration = Duration::from_millis(250);

pub(crate) fn recover_owned_services(paths: &ApplicationPaths) -> AppResult<usize> {
    let mut recovered = BTreeSet::new();
    #[cfg(windows)]
    recovered.extend(recover_owned_windows_guards(paths)?);
    let deadline = Instant::now() + RECOVERY_SCAN_TIMEOUT;
    let mut consecutive_empty_scans = 0;
    while consecutive_empty_scans < 2 {
        if Instant::now() >= deadline {
            return Err(AppError::new("serviceRecoveryTimedOut"));
        }
        #[cfg(target_os = "macos")]
        let groups = owned_service_groups(paths)?;
        #[cfg(windows)]
        let groups = owned_service_processes(paths)?;
        #[cfg(all(not(target_os = "macos"), not(windows)))]
        let groups = BTreeSet::new();

        if groups.is_empty() {
            consecutive_empty_scans += 1;
            thread::sleep(Duration::from_millis(50));
            continue;
        }
        consecutive_empty_scans = 0;
        for group in groups {
            #[cfg(windows)]
            stop_owned_windows_service(group, paths)?;
            #[cfg(target_os = "macos")]
            {
                // Narrow the unavoidable PID/process-group reuse window by
                // repeating the complete ownership check immediately before
                // signaling the group.
                if !owned_service_groups(paths)?.contains(&group) {
                    continue;
                }
                stop_process_group(group)?;
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            stop_process_group(group)?;
            recovered.insert(group);
        }
    }
    let _ = fs::remove_file(&paths.server_pid);
    Ok(recovered.len())
}

#[cfg(unix)]
fn stop_process_group(group: u32) -> AppResult<()> {
    signal_process_group(group, libc::SIGTERM)?;
    if wait_for_process_group(group, RECOVERY_TERM_TIMEOUT) {
        return Ok(());
    }
    signal_process_group(group, libc::SIGKILL)?;
    if wait_for_process_group(group, RECOVERY_KILL_TIMEOUT) {
        return Ok(());
    }
    Err(AppError::new("serviceProcessTreeStillRunning").value("processId", group))
}

#[cfg(unix)]
fn signal_process_group(group: u32, signal: libc::c_int) -> AppResult<()> {
    let group = i32::try_from(group).map_err(|_| AppError::new("serviceRecoveryFailed"))?;
    let result = unsafe { libc::kill(-group, signal) };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(AppError::io("serviceRecoveryFailed", &error))
    }
}

#[cfg(unix)]
fn wait_for_process_group(group: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !process_group_alive(group) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn process_group_alive(group: u32) -> bool {
    let Ok(group) = i32::try_from(group) else {
        return false;
    };
    let result = unsafe { libc::kill(-group, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(target_os = "macos")]
fn process_group_alive(group: u32) -> bool {
    let Ok(group) = i32::try_from(group) else {
        return false;
    };
    let mut capacity = 256_usize;
    loop {
        let mut pids = vec![0_i32; capacity];
        let Ok(buffer_size) = i32::try_from(pids.len() * size_of::<i32>()) else {
            return true;
        };
        let count =
            unsafe { libc::proc_listpgrppids(group, pids.as_mut_ptr().cast(), buffer_size) };
        if count <= 0 {
            return false;
        }
        let count = count as usize;
        if count >= capacity && capacity < 131_072 {
            capacity *= 2;
            continue;
        }
        pids.truncate(count.min(capacity));
        return pids.into_iter().any(|pid| {
            let Ok(pid) = u32::try_from(pid) else {
                return false;
            };
            process_status(pid).is_some_and(|status| status != libc::SZOMB)
        });
    }
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
struct ProcessIdentity {
    pid: u32,
    uid: u32,
    group: u32,
    zombie: bool,
    arguments: Vec<OsString>,
}

#[cfg(target_os = "macos")]
fn owned_service_groups(paths: &ApplicationPaths) -> AppResult<BTreeSet<u32>> {
    let expected_node = fs::canonicalize(&paths.node_bin)
        .map_err(|error| AppError::io("serviceRecoveryFailed", &error))?;
    let expected_dsh = fs::canonicalize(&paths.dsh_bin)
        .map_err(|error| AppError::io("serviceRecoveryFailed", &error))?;
    let current_executable = std::env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|error| AppError::io("serviceRecoveryFailed", &error))?;
    let expected_home = fs::canonicalize(&paths.app_home)
        .map_err(|error| AppError::io("serviceRecoveryFailed", &error))?;
    let current_group = unsafe { libc::getpgrp() };
    let current_uid = unsafe { libc::geteuid() };
    let mut groups = BTreeSet::new();

    for pid in list_processes()? {
        let Some(executable) = process_path(pid) else {
            continue;
        };
        if fs::canonicalize(&executable).ok().as_ref() != Some(&expected_node) {
            continue;
        }
        let Some(identity) = process_identity(pid) else {
            if process_path(pid).is_none() || process_is_zombie(pid) {
                continue;
            }
            return Err(AppError::new("serviceOwnershipUnverifiable").value("processId", pid));
        };
        if identity.zombie {
            continue;
        }
        if identity.uid != current_uid || !managed_dsh_arguments(&identity.arguments, &expected_dsh)
        {
            continue;
        }
        let group = i32::try_from(identity.group)
            .map_err(|_| AppError::new("serviceOwnershipUnverifiable"))?;
        if group <= 1 || group == current_group {
            return Err(
                AppError::new("serviceOwnershipUnverifiable").value("processId", identity.pid)
            );
        }
        if identity.group != identity.pid {
            match settled_guard_group_leader_state(
                identity.group,
                &current_executable,
                &expected_home,
            ) {
                GuardLeaderState::Owned | GuardLeaderState::Gone => {}
                GuardLeaderState::Unverifiable => {
                    return Err(AppError::new("serviceOwnershipUnverifiable")
                        .value("processId", identity.pid));
                }
            }
        }
        groups.insert(identity.group);
    }
    Ok(groups)
}

#[cfg(target_os = "macos")]
fn managed_dsh_arguments(arguments: &[OsString], expected_dsh: &Path) -> bool {
    arguments.len() >= 3
        && fs::canonicalize(Path::new(&arguments[1])).ok().as_deref() == Some(expected_dsh)
        && arguments[2].as_bytes() == b"web"
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy)]
enum GuardLeaderState {
    Owned,
    Gone,
    Unverifiable,
}

#[cfg(target_os = "macos")]
fn settled_guard_group_leader_state(
    group: u32,
    current_executable: &Path,
    expected_home: &Path,
) -> GuardLeaderState {
    let deadline = Instant::now() + RECOVERY_IDENTITY_SETTLE_TIMEOUT;
    loop {
        let state = guard_group_leader_state(group, current_executable, expected_home);
        if !matches!(state, GuardLeaderState::Unverifiable) || Instant::now() >= deadline {
            return state;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(target_os = "macos")]
fn guard_group_leader_state(
    group: u32,
    current_executable: &Path,
    expected_home: &Path,
) -> GuardLeaderState {
    if process_is_zombie(group) {
        return GuardLeaderState::Gone;
    }
    let Some(executable) = process_path(group) else {
        return if !process_exists(group) {
            GuardLeaderState::Gone
        } else {
            GuardLeaderState::Unverifiable
        };
    };
    if fs::canonicalize(executable).ok().as_deref() != Some(current_executable) {
        return GuardLeaderState::Unverifiable;
    }
    if process_arguments(group)
        .is_some_and(|arguments| managed_macos_guard_arguments(&arguments, expected_home))
    {
        GuardLeaderState::Owned
    } else {
        GuardLeaderState::Unverifiable
    }
}

#[cfg(target_os = "macos")]
fn managed_macos_guard_arguments(arguments: &[OsString], expected_home: &Path) -> bool {
    matches!(arguments, [_, guard, home_flag, home]
        if guard.as_bytes() == SERVICE_GUARD_ARGUMENT.as_bytes()
            && home_flag.as_bytes() == SERVICE_GUARD_HOME_ARGUMENT.as_bytes()
            && fs::canonicalize(Path::new(home)).ok().as_deref() == Some(expected_home))
        || matches!(arguments, [_, guard, home_flag, home, free_port]
            if guard.as_bytes() == SERVICE_GUARD_ARGUMENT.as_bytes()
                && home_flag.as_bytes() == SERVICE_GUARD_HOME_ARGUMENT.as_bytes()
                && fs::canonicalize(Path::new(home)).ok().as_deref() == Some(expected_home)
                && free_port.as_bytes() == SERVICE_GUARD_FREE_PORT_ARGUMENT.as_bytes())
}

#[cfg(target_os = "macos")]
fn process_exists(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(target_os = "macos")]
fn list_processes() -> AppResult<Vec<u32>> {
    let mut capacity = 4_096_usize;
    loop {
        let mut pids = vec![0_i32; capacity];
        let buffer_size = i32::try_from(pids.len() * size_of::<i32>())
            .map_err(|_| AppError::new("serviceRecoveryFailed"))?;
        let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), buffer_size) };
        if count < 0 {
            return Err(AppError::io(
                "serviceRecoveryFailed",
                &std::io::Error::last_os_error(),
            ));
        }
        let count = count as usize;
        if count >= capacity {
            capacity = capacity
                .checked_mul(2)
                .filter(|capacity| *capacity <= 131_072)
                .ok_or_else(|| AppError::new("serviceRecoveryFailed"))?;
            continue;
        }
        pids.truncate(count);
        return Ok(pids
            .into_iter()
            .filter_map(|pid| u32::try_from(pid).ok())
            .filter(|pid| *pid > 1)
            .collect());
    }
}

#[cfg(target_os = "macos")]
fn process_identity(pid: u32) -> Option<ProcessIdentity> {
    let info = process_bsd_info(pid)?;
    Some(ProcessIdentity {
        pid,
        uid: info.pbi_uid,
        group: info.pbi_pgid,
        zombie: info.pbi_status == libc::SZOMB,
        arguments: process_arguments(pid)?,
    })
}

#[cfg(target_os = "macos")]
fn process_is_zombie(pid: u32) -> bool {
    process_status(pid) == Some(libc::SZOMB)
}

#[cfg(target_os = "macos")]
fn process_status(pid: u32) -> Option<u32> {
    Some(process_bsd_info(pid)?.pbi_status)
}

#[cfg(target_os = "macos")]
fn process_bsd_info(pid: u32) -> Option<libc::proc_bsdinfo> {
    let mut info: libc::proc_bsdinfo = unsafe { zeroed() };
    let size = i32::try_from(size_of::<libc::proc_bsdinfo>()).ok()?;
    let result = unsafe {
        libc::proc_pidinfo(
            i32::try_from(pid).ok()?,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    if result != size {
        return None;
    }
    Some(info)
}

#[cfg(target_os = "macos")]
fn process_path(pid: u32) -> Option<PathBuf> {
    let mut buffer = vec![0_u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let length = unsafe {
        libc::proc_pidpath(
            i32::try_from(pid).ok()?,
            buffer.as_mut_ptr().cast(),
            u32::try_from(buffer.len()).ok()?,
        )
    };
    if length <= 0 {
        return None;
    }
    buffer.truncate(length as usize);
    Some(PathBuf::from(OsString::from_vec(buffer)))
}

#[cfg(target_os = "macos")]
fn process_arguments(pid: u32) -> Option<Vec<OsString>> {
    let pid = i32::try_from(pid).ok()?;
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size = 0_usize;
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || size < size_of::<i32>()
    {
        return None;
    }
    let mut buffer = vec![0_u8; size];
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            buffer.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    buffer.truncate(size);
    parse_process_arguments(&buffer)
}

#[cfg(target_os = "macos")]
fn parse_process_arguments(buffer: &[u8]) -> Option<Vec<OsString>> {
    let argc = i32::from_ne_bytes(buffer.get(..size_of::<i32>())?.try_into().ok()?);
    let argc = usize::try_from(argc).ok()?;
    let mut cursor = size_of::<i32>();
    cursor += buffer.get(cursor..)?.iter().position(|byte| *byte == 0)? + 1;
    while buffer.get(cursor) == Some(&0) {
        cursor += 1;
    }
    let mut arguments = Vec::with_capacity(argc);
    for _ in 0..argc {
        let remaining = buffer.get(cursor..)?;
        let length = remaining.iter().position(|byte| *byte == 0)?;
        arguments.push(OsString::from_vec(remaining[..length].to_vec()));
        cursor += length + 1;
    }
    Some(arguments)
}

#[cfg(windows)]
#[derive(Debug)]
struct WindowsOwnedHandle(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Drop for WindowsOwnedHandle {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(windows)]
fn owned_service_processes(paths: &ApplicationPaths) -> AppResult<BTreeSet<u32>> {
    let expected_node = fs::canonicalize(&paths.node_bin)
        .map_err(|error| AppError::io("serviceRecoveryFailed", &error))?;
    let expected_dsh = fs::canonicalize(&paths.dsh_bin)
        .map_err(|error| AppError::io("serviceRecoveryFailed", &error))?;
    let current_user = current_windows_user_sid()?;
    let mut processes = BTreeSet::new();

    for (pid, _) in windows_process_entries()? {
        if verified_windows_service_handle(pid, &expected_node, &expected_dsh, &current_user)?
            .is_some()
        {
            processes.insert(pid);
        }
    }
    Ok(processes)
}

#[cfg(windows)]
fn verified_windows_service_handle(
    pid: u32,
    expected_node: &Path,
    expected_dsh: &Path,
    current_user: &[u8],
) -> AppResult<Option<WindowsOwnedHandle>> {
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    const SYNCHRONIZE_ACCESS: u32 = 0x0010_0000;
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE_ACCESS,
            0,
            pid,
        )
    };
    if handle.is_null() {
        return Ok(None);
    }
    let handle = WindowsOwnedHandle(handle);
    if windows_handle_exited(&handle)
        .map_err(|error| AppError::io("serviceRecoveryFailed", &error))?
    {
        return Ok(None);
    }
    let Some(executable) = windows_process_path(&handle) else {
        return Ok(None);
    };
    if fs::canonicalize(executable).ok().as_deref() != Some(expected_node) {
        return Ok(None);
    }
    let user = windows_process_user_sid(&handle).map_err(|error| {
        AppError::io("serviceOwnershipUnverifiable", &error).value("processId", pid)
    })?;
    if user != current_user {
        return Ok(None);
    }
    let arguments = windows_process_arguments(&handle).map_err(|error| {
        AppError::io("serviceOwnershipUnverifiable", &error).value("processId", pid)
    })?;
    if !managed_windows_dsh_arguments(&arguments, expected_dsh) {
        return Ok(None);
    }
    open_verified_windows_termination_handle(pid, &handle)
}

#[cfg(windows)]
fn managed_windows_dsh_arguments(arguments: &[OsString], expected_dsh: &Path) -> bool {
    arguments.len() >= 3
        && fs::canonicalize(Path::new(&arguments[1])).ok().as_deref() == Some(expected_dsh)
        && arguments[2] == OsStr::new("web")
}

#[cfg(windows)]
fn recover_owned_windows_guards(paths: &ApplicationPaths) -> AppResult<BTreeSet<u32>> {
    let expected_launcher = std::env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|error| AppError::io("serviceRecoveryFailed", &error))?;
    let expected_home = fs::canonicalize(&paths.app_home)
        .map_err(|error| AppError::io("serviceRecoveryFailed", &error))?;
    let current_user = current_windows_user_sid()?;
    let mut recovered = BTreeSet::new();
    let deadline = Instant::now() + RECOVERY_SCAN_TIMEOUT;
    let mut consecutive_empty_scans = 0;
    while consecutive_empty_scans < 2 {
        if Instant::now() >= deadline {
            return Err(AppError::new("serviceRecoveryTimedOut"));
        }
        let mut guards = BTreeSet::new();
        for (pid, _) in windows_process_entries()? {
            if verified_windows_guard_handle(
                pid,
                &expected_launcher,
                &expected_home,
                &current_user,
            )?
            .is_some()
            {
                guards.insert(pid);
            }
        }
        if guards.is_empty() {
            consecutive_empty_scans += 1;
            thread::sleep(Duration::from_millis(50));
            continue;
        }
        consecutive_empty_scans = 0;
        for pid in guards {
            if let Some(handle) = verified_windows_guard_handle(
                pid,
                &expected_launcher,
                &expected_home,
                &current_user,
            )? {
                stop_verified_windows_tree(pid, handle)?;
                recovered.insert(pid);
            }
        }
    }
    Ok(recovered)
}

#[cfg(windows)]
fn verified_windows_guard_handle(
    pid: u32,
    expected_launcher: &Path,
    expected_home: &Path,
    current_user: &[u8],
) -> AppResult<Option<WindowsOwnedHandle>> {
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcessId, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    if pid == unsafe { GetCurrentProcessId() } {
        return Ok(None);
    }
    const SYNCHRONIZE_ACCESS: u32 = 0x0010_0000;
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE_ACCESS,
            0,
            pid,
        )
    };
    if handle.is_null() {
        return Ok(None);
    }
    let handle = WindowsOwnedHandle(handle);
    if windows_handle_exited(&handle)
        .map_err(|error| AppError::io("serviceRecoveryFailed", &error))?
    {
        return Ok(None);
    }
    let Some(executable) = windows_process_path(&handle) else {
        return Ok(None);
    };
    if fs::canonicalize(executable).ok().as_deref() != Some(expected_launcher) {
        return Ok(None);
    }
    let user = windows_process_user_sid(&handle).map_err(|error| {
        AppError::io("serviceOwnershipUnverifiable", &error).value("processId", pid)
    })?;
    if user != current_user {
        return Ok(None);
    }
    let arguments = windows_process_arguments(&handle).map_err(|error| {
        AppError::io("serviceOwnershipUnverifiable", &error).value("processId", pid)
    })?;
    if !managed_windows_guard_arguments(&arguments, expected_home) {
        return Ok(None);
    }
    open_verified_windows_termination_handle(pid, &handle)
}

#[cfg(windows)]
fn open_verified_windows_termination_handle(
    pid: u32,
    identity: &WindowsOwnedHandle,
) -> AppResult<Option<WindowsOwnedHandle>> {
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE};

    const SYNCHRONIZE_ACCESS: u32 = 0x0010_0000;
    let handle = unsafe { OpenProcess(PROCESS_TERMINATE | SYNCHRONIZE_ACCESS, 0, pid) };
    if !handle.is_null() {
        return Ok(Some(WindowsOwnedHandle(handle)));
    }

    let error = std::io::Error::last_os_error();
    // Keeping the verified identity handle open pins the process object, so
    // this PID cannot be reused between verification and this second open.
    if windows_handle_exited(identity)
        .map_err(|wait_error| AppError::io("serviceRecoveryFailed", &wait_error))?
    {
        return Ok(None);
    }
    Err(AppError::io("serviceOwnershipUnverifiable", &error).value("processId", pid))
}

#[cfg(windows)]
fn managed_windows_guard_arguments(arguments: &[OsString], expected_home: &Path) -> bool {
    matches!(arguments, [_, guard, home_flag, home]
        if guard == OsStr::new(SERVICE_GUARD_ARGUMENT)
            && home_flag == OsStr::new(SERVICE_GUARD_HOME_ARGUMENT)
            && fs::canonicalize(Path::new(home)).ok().as_deref() == Some(expected_home))
        || matches!(arguments, [_, guard, home_flag, home, free_port]
            if guard == OsStr::new(SERVICE_GUARD_ARGUMENT)
                && home_flag == OsStr::new(SERVICE_GUARD_HOME_ARGUMENT)
                && fs::canonicalize(Path::new(home)).ok().as_deref() == Some(expected_home)
                && free_port == OsStr::new(SERVICE_GUARD_FREE_PORT_ARGUMENT))
}

#[cfg(windows)]
fn stop_owned_windows_service(pid: u32, paths: &ApplicationPaths) -> AppResult<()> {
    let expected_node = fs::canonicalize(&paths.node_bin)
        .map_err(|error| AppError::io("serviceRecoveryFailed", &error))?;
    let expected_dsh = fs::canonicalize(&paths.dsh_bin)
        .map_err(|error| AppError::io("serviceRecoveryFailed", &error))?;
    let current_user = current_windows_user_sid()?;
    let Some(root) =
        verified_windows_service_handle(pid, &expected_node, &expected_dsh, &current_user)?
    else {
        return Ok(());
    };

    stop_verified_windows_tree(pid, root)
}

#[cfg(windows)]
fn stop_verified_windows_tree(pid: u32, root: WindowsOwnedHandle) -> AppResult<()> {
    let mut lineage = BTreeSet::from([pid]);
    let mut descendants = Vec::new();
    let deadline = Instant::now() + RECOVERY_KILL_TIMEOUT;
    discover_windows_descendants(&mut lineage, &mut descendants, deadline)?;

    // Every tracked PID has an open handle and a freshly confirmed parent.
    // Keeping those handles open prevents PID reuse during termination.
    terminate_windows_handle(&root)?;
    for (_, handle) in &descendants {
        terminate_windows_handle(handle)?;
    }
    wait_for_windows_tree(&root, &descendants, deadline)?;

    // Keep all handles open while two final snapshots catch children created
    // between the first snapshot and termination.
    let mut consecutive_empty_scans = 0;
    while consecutive_empty_scans < 2 {
        if Instant::now() >= deadline {
            return Err(AppError::new("serviceProcessTreeStillRunning"));
        }
        let previous_count = descendants.len();
        discover_windows_descendants(&mut lineage, &mut descendants, deadline)?;
        if descendants.len() == previous_count {
            consecutive_empty_scans += 1;
            thread::sleep(Duration::from_millis(50));
            continue;
        }
        consecutive_empty_scans = 0;
        for (_, handle) in &descendants[previous_count..] {
            terminate_windows_handle(handle)?;
        }
        wait_for_windows_tree(&root, &descendants, deadline)?;
    }
    Ok(())
}

#[cfg(windows)]
fn discover_windows_descendants(
    lineage: &mut BTreeSet<u32>,
    descendants: &mut Vec<(u32, WindowsOwnedHandle)>,
    deadline: Instant,
) -> AppResult<()> {
    loop {
        if Instant::now() >= deadline {
            return Err(AppError::new("serviceProcessTreeStillRunning"));
        }
        let entries = windows_process_entries()?;
        let candidates = windows_child_candidates(&entries, lineage);
        if candidates.is_empty() {
            return Ok(());
        }
        let mut added = false;
        for (pid, _) in candidates {
            let Some(handle) = open_windows_termination_handle(pid)? else {
                continue;
            };
            if windows_handle_exited(&handle)
                .map_err(|error| AppError::io("serviceRecoveryFailed", &error))?
            {
                continue;
            }
            let confirmed_parent = windows_process_entries()?
                .into_iter()
                .find_map(|(candidate, parent)| (candidate == pid).then_some(parent));
            if !confirmed_parent.is_some_and(|parent| lineage.contains(&parent)) {
                continue;
            }
            lineage.insert(pid);
            descendants.push((pid, handle));
            added = true;
        }
        if !added {
            return Ok(());
        }
    }
}

#[cfg(windows)]
fn windows_child_candidates(entries: &[(u32, u32)], lineage: &BTreeSet<u32>) -> Vec<(u32, u32)> {
    entries
        .iter()
        .copied()
        .filter(|(pid, parent)| {
            *pid > 1 && pid != parent && !lineage.contains(pid) && lineage.contains(parent)
        })
        .collect()
}

#[cfg(windows)]
fn open_windows_termination_handle(pid: u32) -> AppResult<Option<WindowsOwnedHandle>> {
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE};

    const SYNCHRONIZE_ACCESS: u32 = 0x0010_0000;
    let handle = unsafe { OpenProcess(PROCESS_TERMINATE | SYNCHRONIZE_ACCESS, 0, pid) };
    if handle.is_null() {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(87) {
            return Ok(None);
        }
        return Err(AppError::io("serviceRecoveryFailed", &error).value("processId", pid));
    }
    Ok(Some(WindowsOwnedHandle(handle)))
}

#[cfg(windows)]
fn terminate_windows_handle(handle: &WindowsOwnedHandle) -> AppResult<()> {
    if windows_handle_exited(handle)
        .map_err(|error| AppError::io("serviceRecoveryFailed", &error))?
    {
        return Ok(());
    }
    if unsafe { windows_sys::Win32::System::Threading::TerminateProcess(handle.0, 1) } == 0 {
        let error = std::io::Error::last_os_error();
        if windows_handle_exited(handle)
            .map_err(|wait_error| AppError::io("serviceRecoveryFailed", &wait_error))?
        {
            return Ok(());
        }
        return Err(AppError::io("serviceRecoveryFailed", &error));
    }
    Ok(())
}

#[cfg(windows)]
fn wait_for_windows_tree(
    root: &WindowsOwnedHandle,
    descendants: &[(u32, WindowsOwnedHandle)],
    deadline: Instant,
) -> AppResult<()> {
    loop {
        let root_exited = windows_handle_exited(root)
            .map_err(|error| AppError::io("serviceRecoveryFailed", &error))?;
        let descendants_exited = descendants
            .iter()
            .try_fold(true, |all_exited, (_, handle)| {
                windows_handle_exited(handle).map(|exited| all_exited && exited)
            });
        if root_exited
            && descendants_exited.map_err(|error| AppError::io("serviceRecoveryFailed", &error))?
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(AppError::new("serviceProcessTreeStillRunning"));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(windows)]
fn windows_process_entries() -> AppResult<Vec<(u32, u32)>> {
    use windows_sys::Win32::{
        Foundation::{ERROR_NO_MORE_FILES, INVALID_HANDLE_VALUE},
        System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        },
    };

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(AppError::io(
            "serviceRecoveryFailed",
            &std::io::Error::last_os_error(),
        ));
    }
    let snapshot = WindowsOwnedHandle(snapshot);
    let mut entry = PROCESSENTRY32W::default();
    entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
    if unsafe { Process32FirstW(snapshot.0, &mut entry) } == 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
            return Ok(Vec::new());
        }
        return Err(AppError::io("serviceRecoveryFailed", &error));
    }
    let mut entries = Vec::new();
    loop {
        if entry.th32ProcessID > 1 {
            entries.push((entry.th32ProcessID, entry.th32ParentProcessID));
        }
        if unsafe { Process32NextW(snapshot.0, &mut entry) } == 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                break;
            }
            return Err(AppError::io("serviceRecoveryFailed", &error));
        }
    }
    Ok(entries)
}

#[cfg(windows)]
fn windows_process_path(handle: &WindowsOwnedHandle) -> Option<PathBuf> {
    use windows_sys::Win32::System::Threading::QueryFullProcessImageNameW;

    let mut path = vec![0_u16; 32_768];
    let mut length = u32::try_from(path.len()).ok()?;
    if unsafe { QueryFullProcessImageNameW(handle.0, 0, path.as_mut_ptr(), &mut length) } == 0 {
        return None;
    }
    path.truncate(length as usize);
    Some(PathBuf::from(OsString::from_wide(&path)))
}

#[cfg(windows)]
fn windows_process_arguments(handle: &WindowsOwnedHandle) -> std::io::Result<Vec<OsString>> {
    use windows_sys::{
        Wdk::System::Threading::{NtQueryInformationProcess, ProcessCommandLineInformation},
        Win32::{Foundation::UNICODE_STRING, UI::Shell::CommandLineToArgvW},
    };

    let mut required = 0_u32;
    unsafe {
        NtQueryInformationProcess(
            handle.0,
            ProcessCommandLineInformation,
            ptr::null_mut(),
            0,
            &mut required,
        );
    }
    if required < size_of::<UNICODE_STRING>() as u32 || required > 1_048_576 {
        return Err(std::io::Error::other(
            "invalid process command-line buffer size",
        ));
    }
    let word_count = (required as usize).div_ceil(size_of::<usize>());
    let mut storage = vec![0_usize; word_count];
    let status = unsafe {
        NtQueryInformationProcess(
            handle.0,
            ProcessCommandLineInformation,
            storage.as_mut_ptr().cast::<c_void>(),
            required,
            &mut required,
        )
    };
    if status < 0 {
        return Err(std::io::Error::other(format!(
            "NtQueryInformationProcess failed with status {status:#x}"
        )));
    }
    let command_line = unsafe { &*storage.as_ptr().cast::<UNICODE_STRING>() };
    let byte_length = usize::from(command_line.Length);
    if byte_length % size_of::<u16>() != 0 || command_line.Buffer.is_null() {
        return Err(std::io::Error::other("invalid process command-line string"));
    }
    let start = storage.as_ptr() as usize;
    let end = start
        .checked_add(storage.len() * size_of::<usize>())
        .ok_or_else(|| std::io::Error::other("invalid process command-line range"))?;
    let string_start = command_line.Buffer as usize;
    let string_end = string_start
        .checked_add(byte_length)
        .ok_or_else(|| std::io::Error::other("invalid process command-line range"))?;
    if string_start < start || string_end > end {
        return Err(std::io::Error::other(
            "process command-line pointer was outside its buffer",
        ));
    }
    let mut wide = unsafe {
        std::slice::from_raw_parts(command_line.Buffer, byte_length / size_of::<u16>()).to_vec()
    };
    wide.push(0);
    let mut count = 0_i32;
    let arguments = unsafe { CommandLineToArgvW(wide.as_ptr(), &mut count) };
    if arguments.is_null() || count < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let arguments_guard = WindowsLocalMemory(arguments.cast());
    let arguments = unsafe { std::slice::from_raw_parts(arguments, count as usize) }
        .iter()
        .map(|argument| {
            let mut length = 0;
            unsafe {
                while *argument.add(length) != 0 {
                    length += 1;
                }
                OsString::from_wide(std::slice::from_raw_parts(*argument, length))
            }
        })
        .collect();
    drop(arguments_guard);
    Ok(arguments)
}

#[cfg(windows)]
struct WindowsLocalMemory(*mut c_void);

#[cfg(windows)]
impl Drop for WindowsLocalMemory {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::LocalFree(self.0);
        }
    }
}

#[cfg(windows)]
fn current_windows_user_sid() -> AppResult<Vec<u8>> {
    let handle = unsafe { windows_sys::Win32::System::Threading::GetCurrentProcess() };
    windows_user_sid(handle).map_err(|error| AppError::io("serviceRecoveryFailed", &error))
}

#[cfg(windows)]
fn windows_process_user_sid(handle: &WindowsOwnedHandle) -> std::io::Result<Vec<u8>> {
    windows_user_sid(handle.0)
}

#[cfg(windows)]
fn windows_user_sid(process: windows_sys::Win32::Foundation::HANDLE) -> std::io::Result<Vec<u8>> {
    use windows_sys::Win32::{
        Foundation::HANDLE,
        Security::{
            CopySid, GetLengthSid, GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser,
        },
        System::Threading::OpenProcessToken,
    };

    let mut token: HANDLE = ptr::null_mut();
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let token = WindowsOwnedHandle(token);
    let mut required = 0_u32;
    unsafe {
        GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut required);
    }
    if required < size_of::<TOKEN_USER>() as u32 {
        return Err(std::io::Error::last_os_error());
    }
    let word_count = (required as usize).div_ceil(size_of::<usize>());
    let mut storage = vec![0_usize; word_count];
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            storage.as_mut_ptr().cast(),
            required,
            &mut required,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let token_user = unsafe { &*storage.as_ptr().cast::<TOKEN_USER>() };
    let sid_length = unsafe { GetLengthSid(token_user.User.Sid) };
    if sid_length == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut sid = vec![0_u8; sid_length as usize];
    if unsafe { CopySid(sid_length, sid.as_mut_ptr().cast(), token_user.User.Sid) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(sid)
}

#[cfg(windows)]
fn windows_handle_exited(handle: &WindowsOwnedHandle) -> std::io::Result<bool> {
    use windows_sys::Win32::Foundation::{WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};

    match unsafe { windows_sys::Win32::System::Threading::WaitForSingleObject(handle.0, 0) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        WAIT_FAILED => Err(std::io::Error::last_os_error()),
        result => Err(std::io::Error::other(format!(
            "unexpected process wait result {result}"
        ))),
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn descendant_candidates_only_include_children_of_pinned_processes() {
        let entries = [(10, 1), (11, 10), (12, 11), (20, 1), (21, 20)];
        assert_eq!(
            windows_child_candidates(&entries, &BTreeSet::from([10])),
            vec![(11, 10)]
        );
        assert_eq!(
            windows_child_candidates(&entries, &BTreeSet::from([10, 11])),
            vec![(12, 11)]
        );
    }

    #[test]
    fn service_arguments_require_the_exact_managed_script_and_web_command() {
        let temp = tempfile::tempdir().unwrap();
        let managed = temp.path().join("cli.js");
        let other = temp.path().join("other.js");
        fs::write(&managed, b"managed").unwrap();
        fs::write(&other, b"other").unwrap();
        let managed = fs::canonicalize(managed).unwrap();

        assert!(managed_windows_dsh_arguments(
            &[
                OsString::from("node.exe"),
                managed.clone().into_os_string(),
                OsString::from("web"),
            ],
            &managed,
        ));
        assert!(!managed_windows_dsh_arguments(
            &[
                OsString::from("node.exe"),
                other.into_os_string(),
                OsString::from("web"),
            ],
            &managed,
        ));
        assert!(!managed_windows_dsh_arguments(
            &[
                OsString::from("node.exe"),
                managed.clone().into_os_string(),
                OsString::from("doctor"),
            ],
            &managed,
        ));
    }

    #[test]
    fn guard_arguments_reject_unrelated_launcher_processes() {
        let temp = tempfile::tempdir().unwrap();
        let expected_home = temp.path().join("expected");
        let other_home = temp.path().join("other");
        fs::create_dir_all(&expected_home).unwrap();
        fs::create_dir_all(&other_home).unwrap();
        let expected_home = fs::canonicalize(expected_home).unwrap();
        let arguments = [
            OsString::from("launcher.exe"),
            OsString::from(SERVICE_GUARD_ARGUMENT),
            OsString::from(SERVICE_GUARD_HOME_ARGUMENT),
            expected_home.clone().into_os_string(),
        ];
        assert!(managed_windows_guard_arguments(&arguments, &expected_home));

        let mut free_port_arguments = arguments.to_vec();
        free_port_arguments.push(OsString::from(SERVICE_GUARD_FREE_PORT_ARGUMENT));
        assert!(managed_windows_guard_arguments(
            &free_port_arguments,
            &expected_home,
        ));
        assert!(!managed_windows_guard_arguments(
            &arguments,
            &fs::canonicalize(other_home).unwrap(),
        ));
        assert!(!managed_windows_guard_arguments(
            &[
                OsString::from("launcher.exe"),
                OsString::from(SERVICE_GUARD_ARGUMENT),
            ],
            &expected_home,
        ));
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use std::{
        io::Write,
        os::unix::fs::{PermissionsExt, symlink},
        process::{Child, Command, Stdio},
        sync::{Mutex, MutexGuard},
    };

    use tempfile::TempDir;

    use super::*;
    use crate::runtime::{configure_process_group, terminate_tree};

    struct TestChild(Child);

    struct TestGroup(u32);

    static RECOVERY_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn recovery_test_lock() -> MutexGuard<'static, ()> {
        RECOVERY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    impl Drop for TestGroup {
        fn drop(&mut self) {
            terminate_tree(self.0, true);
        }
    }

    impl TestChild {
        fn id(&self) -> u32 {
            self.0.id()
        }

        fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
            self.0.try_wait()
        }

        fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
            self.0.wait()
        }
    }

    impl Drop for TestChild {
        fn drop(&mut self) {
            if self.0.try_wait().ok().flatten().is_none() {
                terminate_tree(self.0.id(), true);
            }
            let _ = self.0.wait();
        }
    }

    fn fixture() -> (TempDir, ApplicationPaths) {
        let temp = tempfile::tempdir().unwrap();
        let mut paths = ApplicationPaths::from_home(temp.path().join("desktop"));
        paths.ensure_dirs().unwrap();
        paths.node_bin = temp.path().join("managed-node");
        // Link to the system shell instead of copying it: the recovery tests
        // run the fixture executable in shared process groups, and hardened
        // hosts may terminate unrecognized copies of system binaries, which
        // makes the orphaned-service scenario unreachable.
        symlink("/bin/bash", &paths.node_bin).unwrap();
        paths.dsh_bin = temp.path().join("managed-dsh");
        let mut script = fs::File::create(&paths.dsh_bin).unwrap();
        writeln!(script, "#!/bin/sh\nwhile :; do sleep 1; done").unwrap();
        fs::set_permissions(&paths.dsh_bin, fs::Permissions::from_mode(0o700)).unwrap();
        (temp, paths)
    }

    fn spawn_service(node: &Path, dsh: &Path) -> TestChild {
        let mut command = Command::new(node);
        command
            .arg(dsh)
            .arg("web")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        configure_process_group(&mut command);
        TestChild(command.spawn().unwrap())
    }

    fn wait_until_owned(paths: &ApplicationPaths, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if owned_service_groups(paths).unwrap().len() == count {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("managed service was not discovered");
    }

    #[test]
    fn startup_recovery_stops_every_owned_service_group() {
        let _serial = recovery_test_lock();
        let (_temp, paths) = fixture();
        let mut first = spawn_service(&paths.node_bin, &paths.dsh_bin);
        let mut second = spawn_service(&paths.node_bin, &paths.dsh_bin);
        wait_until_owned(&paths, 2);
        assert!(first.try_wait().unwrap().is_none());
        assert!(second.try_wait().unwrap().is_none());
        assert_eq!(owned_service_groups(&paths).unwrap().len(), 2);

        assert_eq!(recover_owned_services(&paths).unwrap(), 2);
        first.wait().unwrap();
        second.wait().unwrap();
        assert!(owned_service_groups(&paths).unwrap().is_empty());
    }

    #[test]
    fn startup_recovery_preserves_a_process_with_different_arguments() {
        let _serial = recovery_test_lock();
        let (temp, paths) = fixture();
        let other = temp.path().join("other-script");
        fs::write(&other, "while :; do sleep 1; done\n").unwrap();
        let mut child = spawn_service(&paths.node_bin, &other);

        assert_eq!(recover_owned_services(&paths).unwrap(), 0);
        assert!(child.try_wait().unwrap().is_none());
        terminate_tree(child.id(), true);
        child.wait().unwrap();
    }

    #[test]
    fn startup_recovery_stops_service_after_its_group_leader_dies() {
        let _serial = recovery_test_lock();
        let (temp, paths) = fixture();
        let child_pid_file = temp.path().join("orphan.pid");
        let mut command = Command::new("/bin/bash");
        command
            .arg("-c")
            .arg("\"$1\" \"$2\" web </dev/null >/dev/null 2>&1 & echo $! > \"$3\"; while :; do sleep 1; done")
            .arg("dsh-test-guard")
            .arg(&paths.node_bin)
            .arg(&paths.dsh_bin)
            .arg(&child_pid_file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        configure_process_group(&mut command);
        let mut leader = command.spawn().unwrap();
        let group = leader.id();
        let _cleanup = TestGroup(group);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !child_pid_file.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        let child_pid = fs::read_to_string(&child_pid_file)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();

        unsafe { libc::kill(i32::try_from(group).unwrap(), libc::SIGKILL) };
        leader.wait().unwrap();
        assert!(process_exists(child_pid));

        assert_eq!(recover_owned_services(&paths).unwrap(), 1);
        assert!(!process_group_alive(group));
    }

    #[test]
    fn guard_arguments_are_scoped_to_the_expected_desktop_home() {
        let temp = tempfile::tempdir().unwrap();
        let expected_home = temp.path().join("expected");
        let other_home = temp.path().join("other");
        fs::create_dir_all(&expected_home).unwrap();
        fs::create_dir_all(&other_home).unwrap();
        let expected_home = fs::canonicalize(expected_home).unwrap();
        let arguments = [
            OsString::from("launcher"),
            OsString::from(SERVICE_GUARD_ARGUMENT),
            OsString::from(SERVICE_GUARD_HOME_ARGUMENT),
            expected_home.clone().into_os_string(),
        ];

        assert!(managed_macos_guard_arguments(&arguments, &expected_home));
        assert!(!managed_macos_guard_arguments(
            &arguments,
            &fs::canonicalize(other_home).unwrap(),
        ));
    }
}
