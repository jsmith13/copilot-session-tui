use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
// Linux waits on a pidfd with poll timeouts instead of sleeping against a deadline, and
// only Windows converts process creation times, so a Linux build uses neither.
#[cfg(not(target_os = "linux"))]
use std::time::Duration;
#[cfg(windows)]
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionOwner {
    pid: u32,
    lock: PathBuf,
}

pub fn active_session_pids(session_dir: &Path) -> Vec<u32> {
    session_owners(session_dir)
        .into_iter()
        .filter(|owner| process_is_running(owner.pid))
        .map(|owner| owner.pid)
        .collect()
}

pub fn terminate_session(session_dir: &Path, confirmed_pids: &[u32]) -> Result<Vec<u32>> {
    let session_id = session_dir
        .file_name()
        .and_then(|name| name.to_str())
        .context("Session directory has no id")?;
    let owners: Vec<SessionOwner> = session_owners(session_dir)
        .into_iter()
        .filter(|owner| process_is_running(owner.pid))
        .collect();
    let mut current_pids: Vec<u32> = owners.iter().map(|owner| owner.pid).collect();
    let mut confirmed_pids = confirmed_pids.to_vec();
    current_pids.sort_unstable();
    confirmed_pids.sort_unstable();
    if current_pids != confirmed_pids {
        anyhow::bail!(
            "Session ownership changed while confirmation was open (was {:?}, now {:?})",
            confirmed_pids,
            current_pids
        );
    }
    let mut validated = owners
        .iter()
        .map(|owner| validate_owner(owner, session_id))
        .collect::<Result<Vec<_>>>()?;
    let mut rechecked = active_session_pids(session_dir);
    rechecked.sort_unstable();
    if rechecked != confirmed_pids {
        anyhow::bail!(
            "Session ownership changed during validation (was {:?}, now {:?})",
            confirmed_pids,
            rechecked
        );
    }
    for owner in &mut validated {
        terminate_owner(owner)?;
    }
    Ok(owners.into_iter().map(|owner| owner.pid).collect())
}

fn session_owners(session_dir: &Path) -> Vec<SessionOwner> {
    let Ok(entries) = fs::read_dir(session_dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let pid = name
                .strip_prefix("inuse.")?
                .strip_suffix(".lock")?
                .parse::<u32>()
                .ok()?;
            Some(SessionOwner {
                pid,
                lock: entry.path(),
            })
        })
        .collect()
}

fn validate_lock(owner: &SessionOwner) -> Result<()> {
    let content = fs::read_to_string(&owner.lock)
        .with_context(|| format!("Failed to read session lock {}", owner.lock.display()))?;
    if content.trim() != owner.pid.to_string() {
        anyhow::bail!(
            "Session lock {} does not belong to PID {}",
            owner.lock.display(),
            owner.pid
        );
    }
    Ok(())
}

#[cfg(windows)]
struct ValidatedOwner {
    pid: u32,
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl Drop for ValidatedOwner {
    fn drop(&mut self) {
        // SAFETY: This handle is owned by the validated owner and closed exactly once.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.handle) };
    }
}

#[cfg(windows)]
fn validate_owner(owner: &SessionOwner, _session_id: &str) -> Result<ValidatedOwner> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, QueryFullProcessImageNameW,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    };

    validate_lock(owner)?;
    const SYNCHRONIZE: u32 = 0x0010_0000;
    // SAFETY: The returned handle is checked and closed on every path below.
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | SYNCHRONIZE,
            0,
            owner.pid,
        )
    };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("Cannot inspect session owner PID {}", owner.pid));
    }
    let result = (|| {
        let mut image = vec![0u16; 32_768];
        let mut image_len = image.len() as u32;
        // SAFETY: `image` is writable for `image_len` UTF-16 code units.
        if unsafe { QueryFullProcessImageNameW(handle, 0, image.as_mut_ptr(), &mut image_len) } == 0
        {
            return Err(std::io::Error::last_os_error())
                .context("Failed to identify the session owner process");
        }
        let image = PathBuf::from(String::from_utf16_lossy(&image[..image_len as usize]));
        let executable = image
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if !executable.eq_ignore_ascii_case("copilot.exe") {
            anyhow::bail!(
                "Refusing to take over: PID {} is '{}' rather than Copilot",
                owner.pid,
                image.display()
            );
        }

        let blank = || FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let mut created = blank();
        let mut exited = blank();
        let mut kernel = blank();
        let mut user = blank();
        // SAFETY: All FILETIME pointers refer to valid writable values.
        if unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) }
            == 0
        {
            return Err(std::io::Error::last_os_error())
                .context("Failed to read the session owner start time");
        }
        let process_created = filetime_to_system_time(created)?;
        let lock_time = fs::metadata(&owner.lock)
            .and_then(|metadata| metadata.created().or_else(|_| metadata.modified()))
            .with_context(|| format!("Failed to date session lock {}", owner.lock.display()))?;
        if process_created > lock_time {
            anyhow::bail!(
                "Refusing to take over: PID {} was reused after this session lock was created",
                owner.pid
            );
        }
        Ok(ValidatedOwner {
            pid: owner.pid,
            handle,
        })
    })();
    if result.is_err() {
        // SAFETY: Ownership transfers to ValidatedOwner only on success.
        unsafe { CloseHandle(handle) };
    }
    result
}

#[cfg(windows)]
fn filetime_to_system_time(
    filetime: windows_sys::Win32::Foundation::FILETIME,
) -> Result<SystemTime> {
    const WINDOWS_TO_UNIX_EPOCH_100NS: u64 = 116_444_736_000_000_000;
    let ticks = ((filetime.dwHighDateTime as u64) << 32) | filetime.dwLowDateTime as u64;
    let unix_ticks = ticks
        .checked_sub(WINDOWS_TO_UNIX_EPOCH_100NS)
        .context("Process creation time predates the Unix epoch")?;
    Ok(SystemTime::UNIX_EPOCH + Duration::from_nanos(unix_ticks.saturating_mul(100)))
}

#[cfg(not(windows))]
struct ValidatedOwner {
    pid: u32,
    #[cfg(target_os = "linux")]
    pidfd: std::os::fd::RawFd,
}

#[cfg(all(not(windows), target_os = "linux"))]
impl Drop for ValidatedOwner {
    fn drop(&mut self) {
        // SAFETY: pidfd is owned by this value and closed exactly once.
        unsafe { libc::close(self.pidfd) };
    }
}

#[cfg(all(not(windows), target_os = "linux"))]
fn validate_owner(owner: &SessionOwner, session_id: &str) -> Result<ValidatedOwner> {
    validate_lock(owner)?;
    let validated = {
        // SAFETY: pidfd_open takes a numeric PID and no pointers.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, owner.pid, 0) as i32 };
        if fd < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("Cannot pin session owner PID {}", owner.pid));
        }
        ValidatedOwner {
            pid: owner.pid,
            pidfd: fd,
        }
    };
    let command = fs::read(format!("/proc/{}/cmdline", owner.pid))
        .with_context(|| format!("Cannot inspect session owner PID {}", owner.pid))?;
    let arguments: Vec<&str> = command
        .split(|byte| *byte == 0)
        .filter(|argument| !argument.is_empty())
        .filter_map(|argument| std::str::from_utf8(argument).ok())
        .collect();
    let owns_session = arguments.iter().any(|argument| {
        *argument == format!("--resume={session_id}")
            || *argument == format!("--session-id={session_id}")
    });
    if !owns_session {
        anyhow::bail!(
            "Refusing to take over: PID {} does not name session {}",
            owner.pid,
            session_id
        );
    }
    let executable = fs::read_link(format!("/proc/{}/exe", owner.pid))
        .with_context(|| format!("Cannot identify session owner PID {}", owner.pid))?;
    let name = executable
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if !name.eq_ignore_ascii_case("copilot") && !name.eq_ignore_ascii_case("copilot.exe") {
        anyhow::bail!(
            "Refusing to take over: PID {} is '{}' rather than Copilot",
            owner.pid,
            executable.display()
        );
    }
    Ok(validated)
}

#[cfg(all(not(windows), not(target_os = "linux")))]
fn validate_owner(owner: &SessionOwner, _session_id: &str) -> Result<ValidatedOwner> {
    validate_lock(owner)?;
    anyhow::bail!(
        "Safe session takeover is not supported on this Unix platform (PID {})",
        owner.pid
    )
}

#[cfg(windows)]
fn terminate_owner(owner: &mut ValidatedOwner) -> Result<()> {
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::Threading::{TerminateProcess, WaitForSingleObject};

    // SAFETY: `handle` is the same validated process handle retained from validation.
    if unsafe { TerminateProcess(owner.handle, 1) } == 0 {
        if !process_is_running(owner.pid) {
            return Ok(());
        }
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("Failed to terminate Copilot PID {}", owner.pid));
    }
    // SAFETY: `handle` remains valid for the lifetime of ValidatedOwner.
    if unsafe { WaitForSingleObject(owner.handle, 3_000) } != WAIT_OBJECT_0 {
        anyhow::bail!("Copilot PID {} did not exit within 3 seconds", owner.pid);
    }
    Ok(())
}

#[cfg(all(not(windows), target_os = "linux"))]
fn terminate_owner(owner: &mut ValidatedOwner) -> Result<()> {
    // SAFETY: pidfd_send_signal targets the pinned process represented by pidfd.
    let signalled = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            owner.pidfd,
            libc::SIGTERM,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if signalled != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("Failed to signal Copilot PID {}", owner.pid));
    }
    let mut pollfd = libc::pollfd {
        fd: owner.pidfd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: pollfd points to one initialized descriptor.
    if unsafe { libc::poll(&mut pollfd, 1, 3_000) } <= 0 {
        // SAFETY: The same pidfd is still pinned to the validated process.
        let killed = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                owner.pidfd,
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if killed != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("Failed to kill Copilot PID {}", owner.pid));
        }
        // SAFETY: pollfd remains valid.
        if unsafe { libc::poll(&mut pollfd, 1, 1_000) } <= 0 {
            anyhow::bail!("Copilot PID {} did not exit after SIGKILL", owner.pid);
        }
    }
    Ok(())
}

#[cfg(all(not(windows), not(target_os = "linux")))]
fn terminate_owner(owner: &mut ValidatedOwner) -> Result<()> {
    let pid = owner.pid;
    let status = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .with_context(|| format!("Failed to signal Copilot PID {pid}"))?;
    if !status.success() && process_is_running(pid) {
        anyhow::bail!("Failed to signal Copilot PID {pid}");
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while process_is_running(pid) && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    if process_is_running(pid) {
        anyhow::bail!("Copilot PID {pid} did not exit within 3 seconds");
    }
    Ok(())
}

#[cfg(windows)]
pub(crate) fn process_is_running(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_INVALID_PARAMETER, INVALID_HANDLE_VALUE, STILL_ACTIVE,
    };
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // SAFETY: OpenProcess takes no pointers; GetExitCodeProcess writes one initialized
    // integer; the returned handle is closed below.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return GetLastError() != ERROR_INVALID_PARAMETER;
        }
        let mut exit_code = 0;
        let queried = GetExitCodeProcess(handle, &mut exit_code);
        CloseHandle(handle);
        queried == 0 || exit_code == STILL_ACTIVE as u32
    }
}

#[cfg(all(not(windows), target_os = "linux"))]
pub(crate) fn process_is_running(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(all(not(windows), not(target_os = "linux")))]
pub(crate) fn process_is_running(pid: u32) -> bool {
    // SAFETY: signal 0 checks existence/permission without delivering a signal.
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_pid_comes_from_a_well_formed_live_lock() {
        let temp = tempfile::tempdir().unwrap();
        let pid = std::process::id();
        fs::write(
            temp.path().join(format!("inuse.{pid}.lock")),
            pid.to_string(),
        )
        .unwrap();
        fs::write(temp.path().join("inuse.not-a-pid.lock"), "nope").unwrap();

        assert_eq!(active_session_pids(temp.path()), vec![pid]);
    }

    #[test]
    fn takeover_refuses_a_lock_owned_by_this_non_copilot_test_process() {
        let temp = tempfile::tempdir().unwrap();
        let pid = std::process::id();
        fs::write(
            temp.path().join(format!("inuse.{pid}.lock")),
            pid.to_string(),
        )
        .unwrap();

        let error = terminate_session(temp.path(), &[pid])
            .unwrap_err()
            .to_string();

        assert!(
            error.contains("rather than Copilot") || error.contains("does not name session"),
            "got {error}"
        );
        assert!(
            process_is_running(pid),
            "validation must run before termination"
        );
    }

    #[test]
    fn takeover_is_bound_to_the_pid_set_the_user_confirmed() {
        let temp = tempfile::tempdir().unwrap();
        let pid = std::process::id();
        fs::write(
            temp.path().join(format!("inuse.{pid}.lock")),
            pid.to_string(),
        )
        .unwrap();

        let error = terminate_session(temp.path(), &[pid.wrapping_add(1)])
            .unwrap_err()
            .to_string();

        assert!(error.contains("ownership changed"), "got {error}");
        assert!(process_is_running(pid));
    }

    #[cfg(windows)]
    #[test]
    fn validated_windows_owner_is_terminated_through_its_pinned_handle() {
        let temp = tempfile::tempdir().unwrap();
        let session = temp.path().join("probe-session");
        fs::create_dir(&session).unwrap();
        let executable = temp.path().join("copilot.exe");
        fs::copy(std::env::var_os("COMSPEC").expect("COMSPEC"), &executable).unwrap();
        let mut child = std::process::Command::new(&executable)
            .args(["/c", "ping -n 30 127.0.0.1 >nul"])
            .spawn()
            .unwrap();
        let pid = child.id();
        fs::write(session.join(format!("inuse.{pid}.lock")), pid.to_string()).unwrap();

        let terminated = terminate_session(&session, &[pid]).unwrap();

        assert_eq!(terminated, vec![pid]);
        assert!(child.try_wait().unwrap().is_some());
        assert!(!process_is_running(pid));
    }
}
