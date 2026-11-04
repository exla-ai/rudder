//! Removes the per-App temp dirs (`test_default_cwd`) that exited test
//! processes left behind. Tests have no process-wide teardown, so without this
//! they were never removed (18k of them filled the shared TMPDIR).

pub(crate) fn sweep_dead_test_cwds() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|name| name.strip_prefix("rudder-test-cwd-"))
            .and_then(|rest| rest.split('-').next())
            .and_then(|pid| pid.parse::<libc::pid_t>().ok())
        else {
            continue;
        };
        // Signal 0 only checks existence; EPERM means alive under another user.
        let alive = unsafe { libc::kill(pid, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        if pid > 0 && !alive && entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn sweep_removes_dirs_of_exited_test_processes_and_keeps_live_ones() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        let temp = std::env::temp_dir();
        let stale = temp.join(format!("rudder-test-cwd-{dead}-0"));
        let live = temp.join(format!("rudder-test-cwd-{}-sweep-check", std::process::id()));
        std::fs::create_dir_all(stale.join(".rudder")).unwrap();
        std::fs::create_dir_all(&live).unwrap();
        super::sweep_dead_test_cwds();
        assert!(!stale.exists(), "an exited test process's dir is removed");
        assert!(live.exists(), "this live process's dirs are kept");
        std::fs::remove_dir_all(&live).unwrap();
    }
}
